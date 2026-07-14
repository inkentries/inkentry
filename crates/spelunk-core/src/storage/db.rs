use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// Wraps the SQLite connection and provides typed access to the schema.
/// Methods are implemented across sub-modules in the `storage` package.
pub struct Database {
    pub(super) conn: Connection,
}

/// Latest local schema version. Append-only: never renumber an existing step.
/// The runner in `Database::open` gates each migration on this via
/// `PRAGMA user_version`; steps are numbered in the order they run (the field
/// order), not filename order.
pub(super) const CURRENT_SCHEMA_VERSION: i32 = 14;

/// One entry in the migration runner: (target version, migration body).
type MigrationStep = (i32, fn(&Database) -> Result<()>);

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

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        let db = Self { conn };
        db.run_migrations()?;
        Ok(db)
    }

    /// Forward-only migration runner gated on `PRAGMA user_version`.
    ///
    /// A `user_version=0` DB is either brand-new (no user tables) or a
    /// pre-`user_version` field DB. New DBs run every step from 1. Field DBs
    /// have their true version inferred from table shapes / the
    /// `schema_int8_embeddings` marker, stamped, and only later steps run —
    /// blindly re-running all steps would drive the guarded 008–010 ALTERs
    /// through their `duplicate column name` branch needlessly. Each step is
    /// idempotent, so a conservative (one-low) inference stays safe.
    fn run_migrations(&self) -> Result<()> {
        let mut version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("reading user_version")?;

        if version == 0 && !self.is_fresh_db()? {
            version = self.infer_legacy_version()?;
        }

        // Each entry: (target_version, migration body). Ordered by call order.
        // Append new steps at the end; never renumber.
        let steps: &[MigrationStep] = &[
            (1, Self::migrate),
            (2, Self::apply_vector_migration),
            (3, Self::apply_graph_migration),
            (4, Self::apply_spec_migration),
            (5, Self::apply_fts_migration),
            (6, Self::apply_token_count_migration),
            (7, Self::apply_graph_rank_migration),
            (8, Self::apply_summary_migration),
            (9, Self::apply_usage_migration),
            (10, Self::apply_compound_graph_idx_migration),
            (11, Self::apply_conventions_migration),
            (12, Self::apply_dim_upgrade_migration),
            (13, Self::apply_drop_snapshots_migration),
            (14, Self::apply_index_meta_migration),
        ];
        debug_assert_eq!(
            steps.last().map(|(v, _)| *v),
            Some(CURRENT_SCHEMA_VERSION),
            "steps table must end at CURRENT_SCHEMA_VERSION"
        );

        for (target, body) in steps {
            if *target > version {
                body(self)?;
            }
        }
        // user_version is a header i32; the value is a code-controlled constant.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .context("stamping user_version")?;
        Ok(())
    }

    /// True when the file has no user tables — a freshly created DB that must
    /// run every migration from step 1.
    fn is_fresh_db(&self) -> Result<bool> {
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

    /// Infer the schema version of a pre-`user_version` field DB from its table
    /// shapes. Walks the ladder top-down; the first unmet predicate fixes the
    /// version. A conservative (one-low) result is safe: the re-run step is a
    /// no-op guard that then advances the version.
    fn infer_legacy_version(&self) -> Result<i32> {
        let has_table = |name: &str| -> Result<bool> {
            Ok(self
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![name],
                    |_| Ok(()),
                )
                .optional()
                .context("probing table")?
                .is_some())
        };
        let has_index = |name: &str| -> Result<bool> {
            Ok(self
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
                    rusqlite::params![name],
                    |_| Ok(()),
                )
                .optional()
                .context("probing index")?
                .is_some())
        };
        let chunks_has_column = |col: &str| -> Result<bool> {
            let mut stmt = self.conn.prepare("PRAGMA table_info(chunks)")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                if row.get::<_, String>(1)? == col {
                    return Ok(true);
                }
            }
            Ok(false)
        };

        let ladder: [(i32, bool); 14] = [
            (1, has_table("chunks")?),
            (2, has_table("embeddings")?),
            (3, has_table("graph_edges")?),
            (4, has_table("specs")?),
            (5, has_table("chunks_fts")?),
            (6, chunks_has_column("token_count")?),
            (7, chunks_has_column("graph_rank")?),
            (8, chunks_has_column("summary")?),
            (9, has_table("usage")?),
            (10, has_index("graph_edges_source_name_kind")?),
            (11, has_table("conventions")?),
            (12, has_table("schema_int8_embeddings")?),
            (13, !has_table("snapshots")?),
            (14, has_table("index_meta")?),
        ];
        // Highest version whose predicate and all lower ones hold.
        let mut version = 0;
        for (v, satisfied) in ladder {
            if !satisfied {
                break;
            }
            version = v;
        }
        Ok(version)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/001_initial.sql"))
            .context("running base migrations")?;
        Ok(())
    }

    /// Create the sqlite-vec virtual table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_vector_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/002_vectors.sql"))
            .context("running vector migration (is the sqlite-vec extension loaded?)")?;
        Ok(())
    }

    /// Create the graph_edges table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_graph_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/003_graph.sql"))
            .context("running graph migration")?;
        Ok(())
    }

    /// Create the specs and spec_links tables. Idempotent (`IF NOT EXISTS`).
    pub fn apply_spec_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/006_specs.sql"))
            .context("running spec migration")?;
        Ok(())
    }

    /// Create the FTS5 virtual table and sync triggers. Idempotent (`IF NOT EXISTS`).
    /// Also backfills any existing chunks not yet in the FTS index.
    pub fn apply_fts_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/007_fts.sql"))
            .context("running FTS migration")?;
        self.conn
            .execute_batch(
                "INSERT INTO chunks_fts(rowid, name, content, node_type)
                 SELECT id, name, content, node_type FROM chunks
                 WHERE id NOT IN (SELECT rowid FROM chunks_fts);",
            )
            .context("backfilling FTS index")?;
        Ok(())
    }

    /// Add token_count column to chunks table.
    /// `ALTER TABLE` has no `IF NOT EXISTS`; only the already-applied error is
    /// tolerated so a genuine failure propagates out of `Database::open`.
    pub fn apply_token_count_migration(&self) -> Result<()> {
        match self
            .conn
            .execute_batch(include_str!("../../migrations/008_token_counts.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running token_count migration"),
        }
        Ok(())
    }

    /// Add graph_rank column to chunks table.
    pub fn apply_graph_rank_migration(&self) -> Result<()> {
        match self
            .conn
            .execute_batch(include_str!("../../migrations/009_graph_rank.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running graph_rank migration"),
        }
        Ok(())
    }

    /// Add summary column to chunks table.
    pub fn apply_summary_migration(&self) -> Result<()> {
        match self
            .conn
            .execute_batch(include_str!("../../migrations/010_summaries.sql"))
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).context("running summary migration"),
        }
        Ok(())
    }

    /// Create the usage table. Idempotent (`IF NOT EXISTS`).
    pub fn apply_usage_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/011_usage.sql"))
            .context("running usage migration")?;
        Ok(())
    }

    /// Upgrade the sqlite-vec embedding tables from 768-dim (Nomic) to 896-dim (F2LLM-v2-330M).
    ///
    /// Idempotent — guarded by the `schema_v896_embeddings` marker table. On
    /// fresh databases the table is already created at 896-dim by
    /// `apply_vector_migration`, so this is a fast no-op. On existing 768-dim
    /// databases the table is dropped and recreated; a full `spelunk index`
    /// re-run is required afterwards.
    pub fn apply_dim_upgrade_migration(&self) -> Result<()> {
        let already: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_int8_embeddings'",
                [],
                |_| Ok(true),
            )
            .optional()
            .context("checking v896 migration marker")?
            .is_some();
        if already {
            return Ok(());
        }

        // Detect whether existing vec0 tables were created with FLOAT[768].
        let upgrade_needed = |table: &str| -> Result<bool> {
            Ok(self
                .conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![table],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .context("querying sqlite_master")?
                // Any float-typed vector table (768 or 896 dim) is rebuilt as
                // int8[896]; F2LLM embeddings are L2-normalised so int8 is
                // lossless enough for ranking and 4× smaller on disk.
                .map(|sql| sql.contains("FLOAT["))
                .unwrap_or(false))
        };

        if upgrade_needed("embeddings")? {
            self.conn
                .execute_batch(
                    "DROP TABLE IF EXISTS embeddings; \
                     CREATE VIRTUAL TABLE embeddings USING vec0(\
                         chunk_id INTEGER PRIMARY KEY, embedding INT8[896]\
                     );",
                )
                .context("upgrading embeddings table to int8[896]")?;
            tracing::info!(
                "embedding storage upgraded to int8[896] (F2LLM-v2-330M); \
                 re-run `spelunk index` to rebuild"
            );
        }
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_int8_embeddings \
                 (sentinel INTEGER PRIMARY KEY);",
            )
            .context("creating int8 migration marker")?;
        Ok(())
    }

    /// Create compound indexes on graph_edges for LinearRAG mention lookups. Idempotent.
    pub fn apply_compound_graph_idx_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!(
                "../../migrations/018_graph_edges_compound_idx.sql"
            ))
            .context("running compound graph index migration")?;
        Ok(())
    }

    /// Drop the snapshot storage tables.
    ///
    /// `snapshots`/`snapshot_files`/`snapshot_chunks` were created by
    /// `016_snapshots.sql` and `snapshot_embeddings` by
    /// `017_snapshot_vectors.sql`, but nothing ever populated them (`spelunk
    /// search --as-of` always errored with "no snapshot found"). Removed for
    /// v1.0 rather than gated behind a flag. `IF EXISTS` makes this a no-op on
    /// fresh databases, which never create these tables in the first place.
    pub fn apply_drop_snapshots_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/021_drop_snapshots.sql"))
            .context("running drop-snapshots migration")?;
        Ok(())
    }

    /// Create the index_meta KV table (embedding provenance). Idempotent.
    pub fn apply_index_meta_migration(&self) -> Result<()> {
        self.conn
            .execute_batch(include_str!("../../migrations/022_index_meta.sql"))
            .context("running index_meta migration")?;
        Ok(())
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
                 Re-index from scratch: `spelunk index . --force` (or delete .spelunk/index.db)."
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

    /// Insert or replace an embedding for a chunk.
    ///
    /// Takes the raw float vector; it is int8-quantised here for the
    /// `embeddings` `int8[896]` column (see `embeddings::vec_to_int8_blob`).
    pub fn insert_embedding(&self, chunk_id: i64, vector: &[f32]) -> Result<()> {
        let blob = crate::embeddings::vec_to_int8_blob(vector);
        // sqlite-vec treats a raw BLOB as float32; vec_int8() reinterprets the
        // bytes as the int8 vector the column expects.
        self.conn.execute(
            "INSERT OR REPLACE INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![chunk_id, blob],
        )?;
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
    use super::{CURRENT_SCHEMA_VERSION, Database};
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

    /// A DB built by the previous binary reports `user_version = 0` but has all
    /// tables. It must be inferred at the latest version, stamped, and re-run
    /// zero erroring migration bodies.
    #[test]
    fn legacy_fully_migrated_db_is_inferred_and_stamped() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let db = Database::open(tmp.path()).expect("build via runner");
            // Simulate a pre-user_version binary: reset the header stamp.
            db.conn
                .execute_batch("PRAGMA user_version = 0")
                .expect("reset version");
        }
        let db = Database::open(tmp.path()).expect("reopen legacy");
        assert_eq!(
            user_version(&db.conn),
            CURRENT_SCHEMA_VERSION,
            "a fully-migrated legacy DB must be inferred at the latest version"
        );
    }

    /// A partially-migrated legacy DB (chunks without `summary`, no index_meta,
    /// version 0) is inferred at 7 and only the later steps run to reach the
    /// latest version.
    #[test]
    fn partially_migrated_legacy_db_is_inferred_then_completed() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            // Build a real DB, then strip it back to a version-7 shape: drop the
            // later columns/tables so inference lands at 7.
            let db = Database::open(tmp.path()).expect("build");
            db.conn
                .execute_batch(
                    "ALTER TABLE chunks DROP COLUMN summary; \
                     DROP TABLE IF EXISTS usage; \
                     DROP INDEX IF EXISTS graph_edges_source_name_kind; \
                     DROP TABLE IF EXISTS conventions; \
                     DROP TABLE IF EXISTS schema_int8_embeddings; \
                     DROP TABLE IF EXISTS index_meta; \
                     PRAGMA user_version = 0;",
                )
                .expect("strip to v7 shape");
            assert!(super::Database::infer_legacy_version(&db).unwrap() == 7);
        }
        let db = Database::open(tmp.path()).expect("reopen partial");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
        // The later step (index_meta) actually ran.
        assert!(db.embedding_model().unwrap().is_none());
        db.ensure_embedding_model("m").unwrap();
        assert_eq!(db.embedding_model().unwrap().as_deref(), Some("m"));
    }

    /// A genuine failure in the guarded 008–010 ALTERs (not a duplicate column)
    /// propagates out rather than being swallowed. We exercise the guard by
    /// dropping the whole `chunks` table so the ALTER fails with "no such
    /// table", which must surface as an `Err`.
    #[test]
    fn token_count_migration_propagates_non_duplicate_error() {
        register_sqlite_vec();
        let conn = Connection::open_in_memory().unwrap();
        let db = Database { conn };
        // No `chunks` table exists → the ALTER fails with "no such table".
        let err = db
            .apply_token_count_migration()
            .expect_err("missing chunks table must surface as an error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no such table") || msg.contains("token_count migration"),
            "a real migration failure must propagate, got: {msg}"
        );
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
}
