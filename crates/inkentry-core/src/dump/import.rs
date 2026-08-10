//! Applying a verified dump to freshly created stores.
//!
//! Two orderings are load-bearing:
//!
//! * **Entities before relationships.** The dump does not constrain record
//!   order, so this works from the whole file rather than record by record.
//!   Foreign keys are enforced, so an edge written before its endpoints is a
//!   hard failure rather than a latent dangling row.
//! * **No embedding inside the write transaction.** Vectors are not carried in
//!   a dump and are rebuilt afterwards, outside this module: a network call per
//!   row while holding a write lock is not something to repeat.

use anyhow::{Context, Result};

use super::reader::Dump;
use super::record::{Entity, RelationshipKind};
use crate::registry::Registry;
use crate::storage::memory::{MemoryStore, NoteId, uuid_v7_at};

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ImportSummary {
    pub memory_entries: usize,
    pub memory_edges: usize,
    pub supersede_links: usize,
    pub projects: usize,
    pub project_dependencies: usize,
    pub command_usages: usize,
    /// Entries with no embedding once the import commits — which is all of
    /// them, since a dump carries no vectors. Reported so the caller can say
    /// so rather than let semantic recall quietly degrade.
    pub entries_needing_embedding: usize,
}

/// Where each entity landed, indexed the same as `Dump::entities`, so a
/// relationship resolves its endpoints without a second lookup.
enum Landed {
    Memory(NoteId),
    Project(i64),
    CommandUsage,
}

pub struct ImportTargets<'a> {
    pub memory: &'a MemoryStore,
    pub registry: Option<&'a Registry>,
    /// `index.db`, which holds the recorded-command table.
    pub index_db: Option<&'a std::path::Path>,
}

pub fn apply(dump: &Dump, targets: &ImportTargets<'_>) -> Result<ImportSummary> {
    let mut summary = ImportSummary::default();

    // One transaction for the memory store: an import either lands whole or
    // leaves nothing behind.
    targets
        .memory
        .execute_batch("BEGIN IMMEDIATE")
        .context("beginning the import transaction")?;
    let result = (|| -> Result<Vec<Landed>> {
        let mut landed = Vec::with_capacity(dump.entities.len());
        for entity in &dump.entities {
            landed.push(insert_entity(entity, targets, &mut summary)?);
        }
        for rel in &dump.relationships {
            insert_relationship(
                rel.kind,
                &landed[rel.from],
                &landed[rel.to],
                rel.created_at,
                targets,
                &mut summary,
            )?;
        }
        Ok(landed)
    })();

    match result {
        Ok(_) => targets
            .memory
            .execute_batch("COMMIT")
            .context("committing the import transaction")?,
        Err(e) => {
            let _ = targets.memory.execute_batch("ROLLBACK");
            return Err(e);
        }
    }

    summary.entries_needing_embedding = targets
        .memory
        .notes_missing_embeddings(false)
        .context("counting entries still needing an embedding")?
        .len();
    Ok(summary)
}

fn insert_entity(
    entity: &Entity,
    targets: &ImportTargets<'_>,
    summary: &mut ImportSummary,
) -> Result<Landed> {
    match entity {
        Entity::MemoryEntry(e) => {
            // An entry arriving with an identity keeps it verbatim. One
            // arriving without gets a UUIDv7 seeded from its OWN creation
            // time: an import replays a whole back catalogue in one pass, so
            // minting from the wall clock would stamp all of history with a
            // single instant and destroy the ordering v7 exists to carry.
            let uuid = e.uuid.clone().unwrap_or_else(|| uuid_v7_at(e.created_at));
            let id = targets
                .memory
                .import_entry(
                    &uuid,
                    &e.kind,
                    &e.title,
                    &e.body,
                    &e.tags,
                    &e.linked_files,
                    e.created_at,
                    e.status.as_deref().unwrap_or("active"),
                    e.source_ref.as_deref(),
                    e.valid_at,
                    e.invalid_at,
                    e.entity_id.as_deref(),
                    e.remote_id.as_deref(),
                )
                .with_context(|| format!("importing memory entry {:?}", e.title))?;
            summary.memory_entries += 1;
            Ok(Landed::Memory(id))
        }
        Entity::Project(p) => {
            let Some(registry) = targets.registry else {
                return Ok(Landed::Project(0));
            };
            // The store path is derived, never carried: the format omits it
            // precisely because the writer's layout need not be this reader's,
            // and a carried path would be wrong on this side.
            let root = std::path::Path::new(&p.root_path);
            let db = root.join(".inkentry").join("index.db");
            let id = registry
                .register(root, &db)
                .with_context(|| format!("registering project {:?}", p.root_path))?;
            summary.projects += 1;
            Ok(Landed::Project(id))
        }
        Entity::CommandUsage(u) => {
            if let Some(index_db) = targets.index_db {
                record_usage_at_time(index_db, &u.command, u.at);
            }
            summary.command_usages += 1;
            Ok(Landed::CommandUsage)
        }
    }
}

fn insert_relationship(
    kind: RelationshipKind,
    from: &Landed,
    to: &Landed,
    created_at: Option<i64>,
    targets: &ImportTargets<'_>,
    summary: &mut ImportSummary,
) -> Result<()> {
    match (kind, from, to) {
        (RelationshipKind::DependsOn, Landed::Project(from), Landed::Project(to)) => {
            if let Some(registry) = targets.registry {
                registry
                    .add_dep(*from, *to)
                    .context("importing a project dependency")?;
            }
            summary.project_dependencies += 1;
            Ok(())
        }
        (RelationshipKind::Supersedes, Landed::Memory(successor), Landed::Memory(predecessor)) => {
            targets
                .memory
                .import_edge(successor, predecessor, kind.as_str(), created_at)?;
            // Supersession arrives only as a relationship, oriented
            // successor-to-predecessor. The column is the inverse encoding of
            // the same fact, so it is set from the relationship rather than
            // expected in the dump.
            targets
                .memory
                .set_superseded_by(predecessor, successor)
                .context("linking a superseded entry to its successor")?;
            summary.memory_edges += 1;
            summary.supersede_links += 1;
            Ok(())
        }
        (
            RelationshipKind::RelatesTo | RelationshipKind::Contradicts,
            Landed::Memory(a),
            Landed::Memory(b),
        ) => {
            targets
                .memory
                .import_edge(a, b, kind.as_str(), created_at)?;
            summary.memory_edges += 1;
            Ok(())
        }
        _ => anyhow::bail!(
            "a {} relationship links entities it cannot link; the dump is not internally \
             consistent. Refusing to import any of it.",
            kind.as_str()
        ),
    }
}

fn record_usage_at_time(index_db: &std::path::Path, command: &str, at: i64) {
    let Ok(conn) = rusqlite::Connection::open(index_db) else {
        return;
    };
    let _ = conn.execute(
        "INSERT INTO usage (command, called_at) VALUES (?1, ?2)",
        rusqlite::params![command, at],
    );
}
