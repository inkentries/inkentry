// Unlike `memory.db`, a step may ask to rebuild instead of migrating in place,
// for a change that invalidates stored data outright. That is resolved in
// `db.rs`, not in the shared ladder, so `memory.db`, which must never rebuild,
// can share the ladder unchanged.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::storage::migration_ladder::MigrationStep;

#[derive(Clone, Copy)]
pub(super) enum IndexMigrationKind {
    Migrate(MigrationStep),
    // A `Rebuild` anywhere between the found version and the target makes the
    // store rebuild once instead of running any step in that range.
    // Unconstructed outside tests: no registered step rebuilds yet.
    #[allow(dead_code)]
    Rebuild,
}

// Append only: number each step for the version it produces, and never
// renumber or reorder an existing one.
pub(super) const INDEX_MIGRATIONS: &[(i32, IndexMigrationKind)] = &[
    (18, IndexMigrationKind::Migrate(add_target_file)),
    (19, IndexMigrationKind::Migrate(rebuild_code_fts)),
    (20, IndexMigrationKind::Migrate(mark_text_only_chunks)),
];

// Existing edges read as unresolved until a graph-only re-extraction fills
// `target_file`. That pass is owed only when the store holds edges, so a fresh
// index climbing the registry owes nothing.
fn add_target_file(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(include_str!("../../migrations/index_018.sql"))?;
    conn.execute(
        "INSERT OR REPLACE INTO index_meta (key, value)
         SELECT ?1, '1' WHERE EXISTS (SELECT 1 FROM graph_edges)",
        rusqlite::params![super::db::GRAPH_EDGES_REEXTRACT],
    )?;
    Ok(())
}

/// The code full-text index rebuilt for retrieval (ADR-103): the DDL is
/// `migrations/index_019.sql`; this backfills the identifier sub-words it adds
/// to `files` and `chunks`. Paths go first because the chunk trigger reads
/// them. Each chunk backfill `UPDATE` fires `chunks_fts_update`, which is what
/// indexes the row, so the new table is complete once the loop ends. Nothing
/// is re-parsed or re-embedded.
fn rebuild_code_fts(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("../../migrations/index_019.sql"))
        .context("applying index_019.sql")?;

    let files: Vec<(i64, String)> = conn
        .prepare("SELECT id, path FROM files")
        .context("preparing the file read")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .context("reading files")?
        .collect::<rusqlite::Result<_>>()
        .context("collecting files")?;
    let mut update_file = conn.prepare("UPDATE files SET path_words = ?1 WHERE id = ?2")?;
    for (id, path) in files {
        let words = crate::search::lexical::identifier_subwords(&path);
        update_file
            .execute(params![(!words.is_empty()).then_some(words), id])
            .with_context(|| format!("backfilling path words for file {id}"))?;
    }

    let rows: Vec<(i64, Option<String>, String, Option<String>)> = conn
        .prepare("SELECT id, name, content, metadata FROM chunks")
        .context("preparing the chunk read")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .context("reading chunks")?
        .collect::<rusqlite::Result<_>>()
        .context("collecting chunks")?;

    let mut update =
        conn.prepare("UPDATE chunks SET name_words = ?1, body_words = ?2 WHERE id = ?3")?;
    for (id, name, content, metadata) in rows {
        let (name_words, body_words) =
            crate::storage::chunk_subwords(name.as_deref(), &content, metadata.as_deref());
        update
            .execute(params![name_words, body_words, id])
            .with_context(|| format!("indexing chunk {id}"))?;
    }
    Ok(())
}

/// Flags the chunks the embed queue now skips (ADR-104). Nothing is deleted:
/// a flagged chunk that was embedded before keeps its vector.
fn mark_text_only_chunks(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("../../migrations/index_020.sql"))
        .context("applying index_020.sql")?;

    let flagged: Vec<i64> = conn
        .prepare(
            "SELECT c.id, f.path, f.language, c.node_type, c.name, c.metadata
             FROM chunks c JOIN files f ON f.id = c.file_id",
        )
        .context("preparing the chunk read")?
        .query_map([], |r| {
            let language: Option<String> = r.get(2)?;
            let name: Option<String> = r.get(4)?;
            let metadata: Option<String> = r.get(5)?;
            let text_only = crate::indexer::embed_scope::is_text_only_row(
                &r.get::<_, String>(1)?,
                language.as_deref().unwrap_or(""),
                &r.get::<_, String>(3)?,
                name.as_deref(),
                metadata.as_deref(),
            );
            Ok(text_only.then_some(r.get::<_, i64>(0)?))
        })
        .context("reading chunks")?
        .filter_map(Result::transpose)
        .collect::<rusqlite::Result<_>>()
        .context("collecting chunks")?;

    let mut update = conn.prepare("UPDATE chunks SET text_only = 1 WHERE id = ?1")?;
    for id in flagged {
        update
            .execute(params![id])
            .with_context(|| format!("flagging chunk {id}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::INDEX_MIGRATIONS;
    use crate::storage::db::{CURRENT_SCHEMA_VERSION, INITIAL_SCHEMA_VERSION};
    use crate::storage::migration_ladder::{MigrationStep, assert_contiguous};

    fn unreachable_step(_: &rusqlite::Connection) -> anyhow::Result<()> {
        unreachable!("a version stamped Rebuild in the production registry has no step body")
    }

    #[test]
    fn the_production_registry_is_contiguous_from_the_first_migratable_version_to_the_current_one()
    {
        // `assert_contiguous` reads only the version, so a placeholder step suffices.
        let versions: Vec<(i32, MigrationStep)> = INDEX_MIGRATIONS
            .iter()
            .map(|&(v, _)| (v, unreachable_step as MigrationStep))
            .collect();
        assert_contiguous(
            &versions,
            INITIAL_SCHEMA_VERSION + 1,
            CURRENT_SCHEMA_VERSION,
        );
    }
}
