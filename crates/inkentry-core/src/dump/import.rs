//! Applying a verified dump to freshly created stores.
//!
//! Three properties are load-bearing:
//!
//! * **Entities before relationships.** The dump does not constrain record
//!   order, so this works from the whole file rather than record by record.
//!   Foreign keys are enforced, so an edge written before its endpoints is a
//!   hard failure rather than a latent dangling row.
//! * **One refusal covers every store.** Memory entries go to `memory.db`,
//!   projects to the registry and recorded commands to `index.db`. "No partial
//!   import" is a claim about the dump, not about one file, so all three are
//!   opened for writing before any of them is written to and are rolled back
//!   together.
//! * **No embedding inside the write transaction.** Vectors are not carried in
//!   a dump and are rebuilt afterwards, outside this module: a network call per
//!   row while holding a write lock is not something to repeat.

use anyhow::{Context, Result};

use super::reader::Dump;
use super::record::{Entity, RelationshipKind};
use crate::registry::Registry;
use crate::storage::memory::{MemoryStore, NoteId, uuid_v7_at};
use crate::storage::note_record::{NoteRecord, now_millis};

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct ImportSummary {
    /// Memory entries that became a row. Not the number of records read: see
    /// the two counts below for where the difference goes.
    pub memory_entries: usize,
    /// Records that shared a convergence key with another record in the same
    /// dump and were folded into it. The store keys entries on that key and
    /// declares it unique, so one key is one row whatever the dump says.
    pub memory_entries_merged: usize,
    /// Records whose entry was already in this store — a second import of the
    /// same dump, or of an overlapping one. Nothing was added for these.
    pub memory_entries_already_present: usize,
    pub memory_edges: usize,
    pub supersede_links: usize,
    pub projects: usize,
    pub project_dependencies: usize,
    pub command_usages: usize,
    /// Entries with no embedding once the import commits — which is all of
    /// them, since a dump carries no vectors. Reported so the caller can say
    /// so rather than let semantic recall quietly degrade.
    pub entries_needing_embedding: usize,
    /// Entries appended to `refs/notes/inkentry`, so they clone with the
    /// repository. Filled in by the caller that owns the carrier write, which
    /// runs after this transaction commits.
    pub memory_entries_carried: usize,
    /// Entries the carrier already held, so nothing was appended for them.
    pub memory_entries_already_carried: usize,
}

/// What one `apply` produced: the counts a user is shown, and the records the
/// git-notes carrier is offered.
pub struct ImportOutcome {
    pub summary: ImportSummary,
    /// Every memory entry the dump carried, in the shape the carrier writes.
    ///
    /// Built for entries that were already rows here as well as for new ones:
    /// the local store and the carrier are separate surfaces, and presence in
    /// one says nothing about presence in the other. Deciding what the carrier
    /// already holds is the carrier's own job.
    pub carrier_records: Vec<NoteRecord>,
}

/// Where each entity landed, indexed the same as `Dump::entities`, so a
/// relationship resolves its endpoints without a second lookup.
enum Landed {
    Memory {
        id: NoteId,
        /// Index into [`ImportOutcome::carrier_records`], so a supersede
        /// relationship can reach the record for either of its endpoints.
        carrier: usize,
    },
    Project(i64),
    CommandUsage,
}

pub struct ImportTargets<'a> {
    pub memory: &'a MemoryStore,
    pub registry: Option<&'a Registry>,
    /// `index.db`, which holds the recorded-command table.
    pub index_db: Option<&'a std::path::Path>,
}

/// Every store this import writes to, held open in a transaction.
///
/// Rolls back on drop, so any `?` on the way through `apply` — a malformed
/// relationship, a constraint, an I/O error — undoes all of them and not just
/// the one that noticed. [`Self::commit`] is the only path that keeps a write.
struct OpenWrites<'a> {
    memory: Option<&'a MemoryStore>,
    registry: Option<&'a Registry>,
    usage: Option<&'a rusqlite::Connection>,
}

impl<'a> OpenWrites<'a> {
    fn begin(
        memory: &'a MemoryStore,
        registry: Option<&'a Registry>,
        usage: Option<&'a rusqlite::Connection>,
    ) -> Result<Self> {
        let mut open = OpenWrites {
            memory: None,
            registry: None,
            usage: None,
        };
        memory
            .execute_batch("BEGIN IMMEDIATE")
            .context("beginning the import transaction on the memory store")?;
        open.memory = Some(memory);
        if let Some(registry) = registry {
            registry
                .execute_batch("BEGIN IMMEDIATE")
                .context("beginning the import transaction on the project registry")?;
            open.registry = Some(registry);
        }
        if let Some(usage) = usage {
            usage
                .execute_batch("BEGIN IMMEDIATE")
                .context("beginning the import transaction on the command-usage store")?;
            open.usage = Some(usage);
        }
        Ok(open)
    }

    /// Commit the memory store first. A commit that fails partway is the one
    /// case this cannot make atomic — two SQLite files have no shared commit —
    /// so the order is chosen for what a retry does: importing the same dump
    /// again converges on the memory side (entries are keyed on a convergence
    /// key) and on the registry side (a project is registered by root path),
    /// while a recorded command is a plain insert that a retry would duplicate.
    /// The one that cannot be repeated safely goes last, where a failure means
    /// it never happened.
    fn commit(mut self) -> Result<()> {
        if let Some(memory) = self.memory.take() {
            memory
                .execute_batch("COMMIT")
                .context("committing the import to the memory store")?;
        }
        if let Some(registry) = self.registry.take() {
            registry
                .execute_batch("COMMIT")
                .context("committing the import to the project registry")?;
        }
        if let Some(usage) = self.usage.take() {
            usage
                .execute_batch("COMMIT")
                .context("committing the import to the command-usage store")?;
        }
        Ok(())
    }
}

impl Drop for OpenWrites<'_> {
    fn drop(&mut self) {
        if let Some(memory) = self.memory.take() {
            let _ = memory.execute_batch("ROLLBACK");
        }
        if let Some(registry) = self.registry.take() {
            let _ = registry.execute_batch("ROLLBACK");
        }
        if let Some(usage) = self.usage.take() {
            let _ = usage.execute_batch("ROLLBACK");
        }
    }
}

pub fn apply(dump: &Dump, targets: &ImportTargets<'_>) -> Result<ImportOutcome> {
    let mut summary = ImportSummary {
        memory_entries_merged: dump.merged_memory_entries,
        ..ImportSummary::default()
    };

    // Both preconditions are checked before anything is opened for writing:
    // a store the dump needs and this machine cannot offer is a reason to
    // refuse the file, not to drop the records that would have gone there.
    let needs_registry = dump
        .entities
        .iter()
        .any(|e| matches!(e, Entity::Project(_)));
    if needs_registry && targets.registry.is_none() {
        anyhow::bail!(
            "this dump carries projects, but the project registry could not be opened. \
             Refusing to import any of it."
        );
    }
    let needs_usage = dump
        .entities
        .iter()
        .any(|e| matches!(e, Entity::CommandUsage(_)));
    let usage_conn = needs_usage
        .then(|| open_usage_store(targets.index_db))
        .transpose()?;

    let open = OpenWrites::begin(targets.memory, targets.registry, usage_conn.as_ref())?;

    let mut landed = Vec::with_capacity(dump.entities.len());
    let mut carrier_records = Vec::new();
    for entity in &dump.entities {
        landed.push(insert_entity(
            entity,
            targets,
            usage_conn.as_ref(),
            &mut summary,
            &mut carrier_records,
        )?);
    }
    for rel in &dump.relationships {
        insert_relationship(
            rel.kind,
            &landed[rel.from],
            &landed[rel.to],
            rel.created_at,
            targets,
            &mut summary,
            &mut carrier_records,
        )?;
    }

    open.commit()?;

    summary.entries_needing_embedding = targets
        .memory
        .notes_missing_embeddings(false)
        .context("counting entries still needing an embedding")?
        .len();
    Ok(ImportOutcome {
        summary,
        carrier_records,
    })
}

/// Open the store recorded commands go to, refusing before any write if it
/// cannot hold them.
///
/// The alternative — swallow the failure per row and count the record as
/// imported anyway — is the shape this whole module exists to avoid.
fn open_usage_store(index_db: Option<&std::path::Path>) -> Result<rusqlite::Connection> {
    let Some(path) = index_db else {
        anyhow::bail!(
            "this dump records command usage, but no index database was supplied to \
             import it into. Refusing to import any of it."
        );
    };
    let conn = rusqlite::Connection::open(path)
        .with_context(|| format!("opening {} for the recorded commands", path.display()))?;
    let has_table: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='usage'",
            [],
            |r| r.get(0),
        )
        .with_context(|| format!("inspecting {}", path.display()))?;
    if has_table == 0 {
        anyhow::bail!(
            "this dump records command usage, but {} has no table to hold it — run \
             `inkentry init` in this project first. Refusing to import any of it.",
            path.display()
        );
    }
    Ok(conn)
}

fn insert_entity(
    entity: &Entity,
    targets: &ImportTargets<'_>,
    usage: Option<&rusqlite::Connection>,
    summary: &mut ImportSummary,
    carrier_records: &mut Vec<NoteRecord>,
) -> Result<Landed> {
    match entity {
        Entity::MemoryEntry(e) => {
            // An entry arriving with an identity keeps it verbatim. One
            // arriving without gets a UUIDv7 seeded from its OWN creation
            // time: an import replays a whole back catalogue in one pass, so
            // minting from the wall clock would stamp all of history with a
            // single instant and destroy the ordering v7 exists to carry.
            let uuid = e.uuid.clone().unwrap_or_else(|| uuid_v7_at(e.created_at));
            // Resolved once and shared with the carrier record below, so the
            // row and the note line can never disagree about which entity
            // this is.
            let entity_id = e.entity_id.clone().unwrap_or_else(|| {
                crate::storage::entity_id::entity_id(&e.kind, &e.title, &e.body)
            });
            let status = e.status.as_deref().unwrap_or("active");
            let (id, created) = targets
                .memory
                .import_entry(
                    &uuid,
                    &e.kind,
                    &e.title,
                    &e.body,
                    &e.tags,
                    &e.linked_files,
                    e.created_at,
                    status,
                    e.source_ref.as_deref(),
                    e.valid_at,
                    e.invalid_at,
                    Some(&entity_id),
                    e.remote_id.as_deref(),
                )
                .with_context(|| format!("importing memory entry {:?}", e.title))?;
            if created {
                summary.memory_entries += 1;
            } else {
                summary.memory_entries_already_present += 1;
            }
            let carrier = carrier_records.len();
            carrier_records.push(carrier_record(e, entity_id, status, carrier));
            Ok(Landed::Memory { id, carrier })
        }
        Entity::Project(p) => {
            let registry = targets.registry.expect(
                "a dump carrying projects is refused before any write when there is no registry",
            );
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
            let conn = usage.expect(
                "a dump carrying recorded commands is refused before any write when there is \
                 nowhere to put them",
            );
            conn.execute(
                "INSERT INTO usage (command, called_at) VALUES (?1, ?2)",
                rusqlite::params![u.command, u.at],
            )
            .with_context(|| format!("importing recorded use of {:?}", u.command))?;
            summary.command_usages += 1;
            Ok(Landed::CommandUsage)
        }
    }
}

/// The dump's entry as the git-notes carrier records it.
///
/// Every field the dump carried is copied verbatim — above all `created_at`,
/// which the carrier's fold orders on. Stamping the wall clock here (as
/// `memory add` rightly does for an entry minted this instant) would make the
/// same entry sort differently on every machine that imported the dump, and
/// the fold would pick a different base copy on each.
///
/// `id` is the exception, because there is nothing to carry: it is the
/// writer's machine-local rowid, which the format documents as **not** an
/// identity, and the entry's real identity is a UUID that does not fit an
/// `i64`. `entity_id` carries the identity instead. The offset keeps one
/// import's records distinct from one another so the `--backend git-notes`
/// read path does not display one id for several entries; nothing else reads
/// it.
fn carrier_record(
    e: &super::record::MemoryEntry,
    entity_id: String,
    status: &str,
    offset: usize,
) -> NoteRecord {
    NoteRecord {
        schema_version: 1,
        id: now_millis().saturating_add(offset as i64),
        kind: e.kind.clone(),
        title: e.title.clone(),
        body: e.body.clone(),
        tags: e.tags.clone(),
        linked_files: e.linked_files.clone(),
        created_at: e.created_at,
        status: status.to_string(),
        source_ref: e.source_ref.clone(),
        valid_at: e.valid_at,
        invalid_at: e.invalid_at,
        // The machine-local rowid link, which no dump carries and this side
        // could not resolve anyway; `superseded_by_entity_id` is the portable
        // encoding and is set from the dump's relationships.
        superseded_by: None,
        remote_id: e.remote_id.clone(),
        entity_id: Some(entity_id),
        superseded_by_entity_id: None,
    }
}

fn insert_relationship(
    kind: RelationshipKind,
    from: &Landed,
    to: &Landed,
    created_at: Option<i64>,
    targets: &ImportTargets<'_>,
    summary: &mut ImportSummary,
    carrier_records: &mut [NoteRecord],
) -> Result<()> {
    match (kind, from, to) {
        (RelationshipKind::DependsOn, Landed::Project(from), Landed::Project(to)) => {
            let registry = targets.registry.expect(
                "a dump carrying projects is refused before any write when there is no registry",
            );
            registry
                .add_dep(*from, *to)
                .context("importing a project dependency")?;
            summary.project_dependencies += 1;
            Ok(())
        }
        (
            RelationshipKind::Supersedes,
            Landed::Memory {
                id: successor,
                carrier: successor_carrier,
            },
            Landed::Memory {
                id: predecessor,
                carrier: predecessor_carrier,
            },
        ) => {
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
            // The same inverse encoding on the carrier, where the portable
            // spelling is the successor's `entity_id`: without it the edge
            // stays in this store and only the entries themselves travel.
            carrier_records[*predecessor_carrier].superseded_by_entity_id =
                Some(carrier_records[*successor_carrier].resolve_entity_id());
            summary.memory_edges += 1;
            summary.supersede_links += 1;
            Ok(())
        }
        (
            RelationshipKind::RelatesTo | RelationshipKind::Contradicts,
            Landed::Memory { id: a, .. },
            Landed::Memory { id: b, .. },
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
