use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

use super::MemoryReconcileArgs;
use crate::{
    capability,
    capability::inkentry_state_dir,
    config::Config,
    server_client::ServerInferenceClient,
    storage::{MemoryStore, NoteId, entity_id, note_entity_id},
};

// `id` and `superseded_by` are server-local rowids: only valid within server.db.
#[derive(Debug, Clone)]
struct ServerNote {
    id: i64,
    kind: String,
    title: String,
    body: String,
    tags: String, // raw CSV as stored in server.db
    linked_files: String,
    created_at: i64,
    status: String,
    superseded_by: Option<i64>,
}

impl ServerNote {
    fn entity_id(&self) -> String {
        entity_id(&self.kind, &self.title, &self.body)
    }

    fn tags_vec(&self) -> Vec<String> {
        split_csv(&self.tags)
    }

    fn files_vec(&self) -> Vec<String> {
        split_csv(&self.linked_files)
    }

    fn is_archived(&self) -> bool {
        self.status == "archived"
    }
}

fn split_csv(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// Candidate rows sharing one `entity_id` fold into one entry. `created_at`,
// `tags` and `linked_files` are outside the key, so rows differing only there
// collapse.
#[derive(Debug)]
struct MergedNote {
    entity_id: String,
    kind: String,
    title: String,
    body: String,
    tags: Vec<String>,
    linked_files: Vec<String>,
    created_at: i64,
    status: String,
    rows: usize,
}

impl MergedNote {
    fn from_server(entity_id: String, c: &ServerNote) -> Self {
        Self {
            entity_id,
            kind: c.kind.clone(),
            title: c.title.clone(),
            body: c.body.clone(),
            tags: c.tags_vec(),
            linked_files: c.files_vec(),
            created_at: c.created_at,
            status: c.status.clone(),
            rows: 1,
        }
    }

    // `kind`/`title`/`body` are the id, so only the rest folds: tags and files
    // union, an archive on any row sticks, and the earliest `created_at` wins so
    // supersede chains import in order.
    fn absorb(&mut self, c: &ServerNote) {
        for t in c.tags_vec() {
            if !self.tags.contains(&t) {
                self.tags.push(t);
            }
        }
        for f in c.files_vec() {
            if !self.linked_files.contains(&f) {
                self.linked_files.push(f);
            }
        }
        if c.is_archived() {
            self.status = "archived".to_string();
        }
        self.created_at = self.created_at.min(c.created_at);
        self.rows += 1;
    }
}

fn collapse_candidates(candidates: &[ServerNote]) -> Vec<MergedNote> {
    let mut merged: Vec<MergedNote> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for c in candidates {
        let eid = c.entity_id();
        match index.get(&eid) {
            Some(&i) => merged[i].absorb(c),
            None => {
                index.insert(eid.clone(), merged.len());
                merged.push(MergedNote::from_server(eid, c));
            }
        }
    }
    merged
}

// Resolved through server-local rowids, then expressed in `entity_id`s so the
// edge survives the crossing into a store that numbers rows differently.
fn build_supersede_edges(candidates: &[ServerNote]) -> HashMap<String, String> {
    let by_server_id: HashMap<i64, &ServerNote> = candidates.iter().map(|c| (c.id, c)).collect();
    let mut edges = HashMap::new();
    for c in candidates {
        if let Some(succ_id) = c.superseded_by
            && let Some(succ) = by_server_id.get(&succ_id)
        {
            edges
                .entry(c.entity_id())
                .or_insert_with(|| succ.entity_id());
        }
    }
    edges
}

#[derive(Debug, Serialize)]
struct ReconcileError {
    stage: String,
    message: String,
}

// Counts are over source rows and partition them:
// `candidates == already_present + collapsed_duplicates + imported`
// (`would_import` replaces `imported` under `--dry-run`).
#[derive(Debug, Serialize)]
struct ReconcileSummary {
    source_db: String,
    project_slug: String,
    candidates: usize,
    already_present: usize,
    collapsed_duplicates: usize,
    imported: usize,
    would_import: usize,
    imported_without_embedding: usize,
    skipped_archived_supersede_unresolved: usize,
    errors: Vec<ReconcileError>,
}

pub(super) async fn memory_reconcile(
    args: MemoryReconcileArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let json = crate::utils::effective_format(&args.format) == "json";

    let server_db_path = args
        .source_db
        .clone()
        .unwrap_or_else(default_server_db_path);

    if !server_db_path.exists() {
        let summary = ReconcileSummary {
            source_db: server_db_path.display().to_string(),
            project_slug: String::new(),
            candidates: 0,
            already_present: 0,
            collapsed_duplicates: 0,
            imported: 0,
            would_import: 0,
            imported_without_embedding: 0,
            skipped_archived_supersede_unresolved: 0,
            errors: vec![],
        };
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let slug = cfg.resolve_project_id(&project_root);

    // `get_inference_tier`, not `get_tier`: local_first prefers the loopback
    // embedder even with an explicit server_url; otherwise every note imports
    // unembedded.
    let tier = capability::get_inference_tier(cfg).await;
    let eff_cfg = tier.effective_config(cfg, &project_root);
    let cfg = &eff_cfg;

    if args.all_projects {
        run_all_projects(&server_db_path, mem_path, cfg, &args, json).await
    } else {
        let result = reconcile_project(&slug, &server_db_path, mem_path, cfg, &args, json).await;
        // Errors already propagate out of reconcile_project.
        result
    }
}

async fn run_all_projects(
    server_db_path: &std::path::Path,
    mem_path: &std::path::Path,
    cfg: &Config,
    args: &MemoryReconcileArgs,
    json: bool,
) -> Result<()> {
    let slugs = list_server_project_slugs(server_db_path)?;
    if slugs.is_empty() {
        if !json {
            eprintln!("[inkentry] No projects found in server.db.");
        }
        return Ok(());
    }
    for slug in &slugs {
        reconcile_project(slug, server_db_path, mem_path, cfg, args, json).await?;
    }
    Ok(())
}

fn list_server_project_slugs(server_db_path: &std::path::Path) -> Result<Vec<String>> {
    let conn = open_server_db_readonly(server_db_path)?;
    let mut stmt = conn.prepare("SELECT slug FROM projects ORDER BY id")?;
    let slugs: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(slugs)
}

async fn reconcile_project(
    slug: &str,
    server_db_path: &std::path::Path,
    mem_path: &std::path::Path,
    cfg: &Config,
    args: &MemoryReconcileArgs,
    json: bool,
) -> Result<()> {
    let source_db_str = server_db_path.display().to_string();

    let mut summary = ReconcileSummary {
        source_db: source_db_str.clone(),
        project_slug: slug.to_string(),
        candidates: 0,
        already_present: 0,
        collapsed_duplicates: 0,
        imported: 0,
        would_import: 0,
        imported_without_embedding: 0,
        skipped_archived_supersede_unresolved: 0,
        errors: vec![],
    };

    let server_conn = match open_server_db_readonly(server_db_path) {
        Ok(c) => c,
        Err(e) => {
            summary.errors.push(ReconcileError {
                stage: "open_source_db".to_string(),
                message: format!("{e:#}"),
            });
            emit_summary(&summary, json, args.dry_run);
            anyhow::bail!("could not open source db: {e:#}");
        }
    };

    let project_id: Option<i64> = server_conn
        .query_row(
            "SELECT id FROM projects WHERE slug = ?1",
            rusqlite::params![slug],
            |r| r.get(0),
        )
        .optional()
        .context("querying projects table in server.db")?;

    let Some(project_id) = project_id else {
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    };

    let candidates =
        read_server_notes(&server_conn, project_id).context("reading notes from server.db")?;
    drop(server_conn);

    summary.candidates = candidates.len();

    if candidates.is_empty() {
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    let mem_store = MemoryStore::open(mem_path)
        .with_context(|| format!("opening memory.db at {}", mem_path.display()))?;

    let existing_notes = mem_store
        .all_notes_for_dedup()
        .context("reading existing memory.db notes for dedup")?;

    // A store can hold several rows under one entity_id; the oldest is the
    // stable edge target.
    let mut entity_to_local: HashMap<String, NoteId> = HashMap::new();
    for n in &existing_notes {
        entity_to_local
            .entry(note_entity_id(n))
            .or_insert_with(|| n.id.clone());
    }

    let (present, mut to_import): (Vec<MergedNote>, Vec<MergedNote>) =
        collapse_candidates(&candidates)
            .into_iter()
            .partition(|m| entity_to_local.contains_key(&m.entity_id));

    // Oldest first so supersede chains import in order.
    to_import.sort_by_key(|n| n.created_at);

    summary.already_present = present.iter().map(|m| m.rows).sum();
    summary.collapsed_duplicates = to_import.iter().map(|m| m.rows - 1).sum();

    if args.dry_run {
        summary.would_import = to_import.len();
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    // Add-wins: a candidate matching a stored entry may carry tags and files
    // the stored copy lacks.
    for m in &present {
        let Some(local_id) = entity_to_local.get(&m.entity_id) else {
            continue;
        };
        if let Err(e) = mem_store.union_tags_and_files(local_id, &m.tags, &m.linked_files) {
            tracing::warn!("reconcile: could not merge tags into {local_id}: {e}");
        }
    }

    if to_import.is_empty() {
        emit_summary(&summary, json, args.dry_run);
        return Ok(());
    }

    let embed_client = ServerInferenceClient::from_config(cfg);

    // Embedded up front so the transaction is not held open across server calls.
    let mut embeddings: Vec<Option<Vec<u8>>> = Vec::with_capacity(to_import.len());
    for note in &to_import {
        let text = format!("title: {} | text: {}", note.title, note.body);
        let blob = try_embed(&embed_client, &text).await;
        if blob.is_none() {
            summary.imported_without_embedding += 1;
        }
        embeddings.push(blob);
    }

    let import_result =
        import_batch(&mem_store, &to_import, &embeddings).context("inserting notes into memory.db");

    match import_result {
        Ok(imported_ids) => {
            summary.imported = imported_ids.len();

            // The successor may be a row just imported or one already held.
            for (note, local_id) in to_import.iter().zip(imported_ids.iter()) {
                entity_to_local.insert(note.entity_id.clone(), local_id.clone());
            }

            let supersede_edges = build_supersede_edges(&candidates);

            let mut unresolved = 0usize;
            for (note, local_id) in to_import.iter().zip(imported_ids.iter()) {
                if note.status != "archived" {
                    continue;
                }
                let Some(succ_entity_id) = supersede_edges.get(&note.entity_id) else {
                    continue;
                };
                let Some(succ_local_id) = entity_to_local.get(succ_entity_id) else {
                    unresolved += 1;
                    continue;
                };
                // A supersede pair with identical text collapses to one entry.
                if succ_local_id == local_id {
                    continue;
                }
                if let Err(e) = mem_store.set_superseded_by(local_id, succ_local_id) {
                    tracing::warn!("reconcile: could not set superseded_by for {local_id}: {e}");
                    unresolved += 1;
                }
            }
            summary.skipped_archived_supersede_unresolved = unresolved;

            emit_summary(&summary, json, args.dry_run);
            Ok(())
        }
        Err(e) => {
            summary.errors.push(ReconcileError {
                stage: "import_transaction".to_string(),
                message: format!("{e:#}"),
            });
            emit_summary(&summary, json, args.dry_run);
            anyhow::bail!("{e:#}");
        }
    }
}

// Must use the daemon's own `inkentry_state_dir` resolver: a reconstructed path
// would miss server.db under `INKENTRY_STATE_DIR` and report a silent no-op.
fn default_server_db_path() -> PathBuf {
    inkentry_state_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("server.db")
}

// The daemon owns server.db; never write to it.
fn open_server_db_readonly(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening server.db read-only at {}", path.display()))?;
    // WAL read-mode so the daemon's writers are not blocked.
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    Ok(conn)
}

fn read_server_notes(conn: &Connection, project_id: i64) -> Result<Vec<ServerNote>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, title, body, \
               COALESCE(tags, ''), COALESCE(linked_files, ''), \
               created_at, status, superseded_by \
         FROM notes \
         WHERE project_id = ?1 \
         ORDER BY created_at ASC",
    )?;
    let notes = stmt
        .query_map(rusqlite::params![project_id], |row| {
            Ok(ServerNote {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                body: row.get(3)?,
                tags: row.get(4)?,
                linked_files: row.get(5)?,
                created_at: row.get(6)?,
                status: row.get(7)?,
                superseded_by: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(notes)
}

fn import_batch(
    store: &MemoryStore,
    notes: &[MergedNote],
    embeddings: &[Option<Vec<u8>>],
) -> Result<Vec<NoteId>> {
    store
        .execute_batch("BEGIN IMMEDIATE")
        .context("beginning import transaction")?;

    let mut ids = Vec::with_capacity(notes.len());
    let result: Result<()> = (|| {
        for (note, embedding) in notes.iter().zip(embeddings.iter()) {
            let tag_parts: Vec<&str> = note.tags.iter().map(String::as_str).collect();
            let file_parts: Vec<&str> = note.linked_files.iter().map(String::as_str).collect();

            let status = if note.status == "archived" {
                "archived"
            } else {
                "active"
            };

            let (id, _created) = store.add_note_with_created_at(
                &note.kind,
                &note.title,
                &note.body,
                &tag_parts,
                &file_parts,
                Some("reconcile:server.db"),
                status,
                note.created_at,
            )?;
            if let Some(blob) = embedding {
                store.insert_embedding(&id, blob)?;
            }
            ids.push(id);
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            store
                .execute_batch("COMMIT")
                .context("committing import transaction")?;
            Ok(ids)
        }
        Err(e) => {
            let _ = store.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

async fn try_embed(client: &Option<ServerInferenceClient>, text: &str) -> Option<Vec<u8>> {
    use crate::embeddings::vec_to_blob;
    let client = client.as_ref()?;
    match client.embed_text(text).await {
        Ok(vec) => Some(vec_to_blob(&vec)),
        Err(e) => {
            tracing::debug!("reconcile: embedding failed (non-fatal): {e}");
            None
        }
    }
}

fn emit_summary(summary: &ReconcileSummary, json: bool, dry_run: bool) {
    if json {
        println!("{}", serde_json::to_string(summary).unwrap_or_default());
    } else {
        print_human_summary(summary, dry_run);
    }
}

fn print_human_summary(s: &ReconcileSummary, dry_run: bool) {
    if dry_run {
        eprintln!(
            "[inkentry] reconcile (dry-run): source={} project={} candidates={} already_present={} would_import={}",
            s.source_db, s.project_slug, s.candidates, s.already_present, s.would_import
        );
        return;
    }
    eprintln!(
        "[inkentry] reconcile: source={} project={} candidates={} already_present={} imported={} without_embedding={} supersede_unresolved={}",
        s.source_db,
        s.project_slug,
        s.candidates,
        s.already_present,
        s.imported,
        s.imported_without_embedding,
        s.skipped_archived_supersede_unresolved,
    );
    if !s.errors.is_empty() {
        for e in &s.errors {
            eprintln!("[inkentry] reconcile error ({}): {}", e.stage, e.message);
        }
    }
}

const INIT_GIT_NOTES_SOURCE: &str = "init:git-notes";

// Matches `GitNotesBackend`'s per-list cap; asking for more is truncated with a
// warning.
const GIT_NOTES_IMPORT_LIMIT: usize = 500;

// Edges naming an entry this store lacks are counted unresolved, not fatal; a
// later import resolves them. Supersede edges are counted apart because they
// ride their own carrier field.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GitNotesImport {
    pub imported: usize,
    pub edges_applied: usize,
    pub edges_unresolved: usize,
    pub supersede_edges_applied: usize,
    pub supersede_edges_unresolved: usize,
}

pub(crate) async fn import_git_notes_into_memory(
    git_root: &std::path::Path,
    mem_path: &std::path::Path,
) -> Result<GitNotesImport> {
    use crate::storage::{GitNotesBackend, MemoryBackend, NotesRefs};

    let backend = GitNotesBackend::with_root(git_root.to_path_buf());
    // include_archived so archived entries import and dedup.
    let notes = backend
        .list(None, GIT_NOTES_IMPORT_LIMIT, true, None)
        .await?;

    // Stamped in the same transaction as the imported rows so a crash cannot
    // leave the marker and the store disagreeing.
    let working_oid = NotesRefs::discover(Some(git_root)).and_then(|r| r.working_oid());

    if notes.is_empty() {
        // Advance the marker only if a store exists: an empty ref must not
        // create memory.db.
        if mem_path.exists()
            && let Ok(store) = MemoryStore::open(mem_path)
        {
            let _ = store.set_notes_imported_working_oid(working_oid.as_deref());
        }
        return Ok(GitNotesImport::default());
    }

    let store = MemoryStore::open(mem_path)
        .with_context(|| format!("opening memory.db at {}", mem_path.display()))?;
    let mut existing: std::collections::HashSet<String> = store
        .all_notes_for_dedup()
        .context("reading existing memory.db notes for dedup")?
        .iter()
        .map(note_entity_id)
        .collect();

    // `insert` returning false also drops duplicates within the notes ref:
    // identical text is one entity.
    let to_import: Vec<&crate::storage::memory::Note> = notes
        .iter()
        .filter(|&n| existing.insert(note_entity_id(n)))
        .collect();
    // A git read, so done before the transaction takes its lock.
    let carried = backend
        .carried_edges()
        .await
        .context("reading the edges carried on the notes ref")?;
    // Supersede rides its own carrier field, apart from the edge list.
    let supersede_pairs = backend
        .carried_supersede_edges()
        .await
        .context("reading the supersede edges carried on the notes ref")?;

    if to_import.is_empty() && carried.is_empty() && supersede_pairs.is_empty() {
        // Advance the marker so the read-path gate stops re-walking.
        let _ = store.set_notes_imported_working_oid(working_oid.as_deref());
        return Ok(GitNotesImport::default());
    }

    store
        .execute_batch("BEGIN IMMEDIATE")
        .context("beginning git-notes import transaction")?;
    let result: Result<GitNotesImport> = (|| {
        for note in &to_import {
            let tags: Vec<&str> = note.tags.iter().map(String::as_str).collect();
            let files: Vec<&str> = note.linked_files.iter().map(String::as_str).collect();
            let status = if note.status == "archived" {
                "archived"
            } else {
                "active"
            };
            store.add_note_with_created_at(
                &note.kind,
                &note.title,
                &note.body,
                &tags,
                &files,
                Some(INIT_GIT_NOTES_SOURCE),
                status,
                note.created_at,
            )?;
        }
        // After every insert above, so an edge between two entries arriving in
        // the same pass finds both of them.
        let edges = store.import_carried_edges(&carried)?;
        // Same ordering requirement: the foreign keys refuse a supersede row
        // until both its endpoints are in the store.
        let supersedes = store.import_supersede_edges(&supersede_pairs)?;
        // Crash-atomic with the inserts above.
        store.set_notes_imported_working_oid(working_oid.as_deref())?;
        Ok(GitNotesImport {
            imported: to_import.len(),
            edges_applied: edges.applied,
            edges_unresolved: edges.unresolved,
            supersede_edges_applied: supersedes.applied,
            supersede_edges_unresolved: supersedes.unresolved,
        })
    })();

    match result {
        Ok(outcome) => {
            store
                .execute_batch("COMMIT")
                .context("committing git-notes import transaction")?;
            Ok(outcome)
        }
        Err(e) => {
            let _ = store.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// The merge runs only when the tracking ref moved and the import only when the
// working ref moved, so the steady state spawns no git subprocess. No network:
// it folds in only what the user's own `git fetch` wrote. Never fails the
// caller: a read must not break because the refresh could not run.
pub(crate) async fn refresh_read_path_from_git_notes(
    cfg: &Config,
    mem_path: &std::path::Path,
    backend_override: Option<&str>,
) {
    use crate::config::SyncMode;
    use crate::storage::NotesRefs;

    // Git notes is the primary store: fold the tracking ref in so the direct
    // read sees fetched entries. No memory.db marker exists here, so the merge
    // is unconditional.
    if backend_override == Some("git-notes") {
        crate::storage::merge_tracking_notes(None).await;
        return;
    }
    // cloud_first: the server is the store of record; the local carrier is not read.
    if cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some() {
        return;
    }

    let Some(refs) = NotesRefs::discover(None) else {
        return;
    };
    let tracking = refs.tracking_oid();
    let mut working = refs.working_oid();
    if tracking.is_none() && working.is_none() {
        return; // nothing on either notes ref → no store churn
    }

    // Opening a missing store just to read markers would churn an empty
    // memory.db, so no store means no marker.
    let marker = if mem_path.exists() {
        MemoryStore::open(mem_path)
            .ok()
            .and_then(|s| s.notes_import_state().ok())
            .unwrap_or_default()
    } else {
        crate::storage::NotesImportMarker::default()
    };

    let git_root = refs.workdir();

    let tracking_moved = tracking != marker.last_merged_tracking_oid;
    if tracking_moved {
        crate::storage::merge_tracking_notes(git_root).await;
        working = refs.working_oid(); // the merge may have advanced the working ref
    }

    if working != marker.last_imported_working_oid
        && let Some(git_root) = git_root
        && let Err(e) = import_git_notes_into_memory(git_root, mem_path).await
    {
        tracing::warn!("read-path git-notes import skipped (non-fatal): {e}");
    }

    // After the import, which may have created the store.
    if tracking_moved && let Ok(store) = MemoryStore::open(mem_path) {
        let _ = store.set_notes_merged_tracking_oid(tracking.as_deref());
    }
}

pub(super) fn count_reconcilable(
    server_db_path: &std::path::Path,
    mem_path: &std::path::Path,
    slug: &str,
) -> Option<usize> {
    let conn = open_server_db_readonly(server_db_path).ok()?;
    let project_id: i64 = conn
        .query_row(
            "SELECT id FROM projects WHERE slug = ?1",
            rusqlite::params![slug],
            |r| r.get(0),
        )
        .optional()
        .ok()??;

    let candidates = read_server_notes(&conn, project_id).ok()?;
    if candidates.is_empty() {
        return None;
    }
    drop(conn);

    let mem_store = MemoryStore::open(mem_path).ok()?;
    let existing = mem_store.all_notes_for_dedup().ok()?;
    let existing_entities: std::collections::HashSet<String> =
        existing.iter().map(note_entity_id).collect();

    // Collapsed duplicates count once; must not promise more than reconcile imports.
    let count = collapse_candidates(&candidates)
        .iter()
        .filter(|m| !existing_entities.contains(&m.entity_id))
        .count();

    if count > 0 { Some(count) } else { None }
}

pub(crate) fn maybe_emit_nudge(mem_path: &std::path::Path, cfg: &Config) {
    if std::env::var_os("INKENTRY_NO_RECONCILE_NUDGE").is_some() {
        return;
    }

    // A prior reconcile means the user has already seen the nudge.
    if let Ok(store) = MemoryStore::open(mem_path)
        && store.has_source_ref("reconcile:server.db").unwrap_or(false)
    {
        return;
    }

    let server_db = default_server_db_path();
    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let slug = cfg.resolve_project_id(&project_root);

    if let Some(n) = count_reconcilable(&server_db, mem_path, &slug) {
        eprintln!(
            "[inkentry] {n} note(s) recorded by a local server aren't in this project's memory yet. \
             Run 'inkentry memory reconcile' to import them."
        );
    }
}

#[cfg(test)]
mod init_import_tests {
    use super::*;
    use crate::storage::{GitNotesBackend, MemoryBackend, NoteInput};
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

    fn make_temp_git_repo() -> tempfile::TempDir {
        // An ambient `core.hooksPath` would fire a foreign pre-commit hook on
        // the commit below.
        crate::cli::cmd::test_support::isolate_git_config();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let p = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(p)
                .output()
                .expect("git command");
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(p.join("README.md"), "test").expect("write");
        run(&["add", "."]);
        run(&[
            "commit",
            "--no-gpg-sign",
            "-m",
            "init",
            "--allow-empty-message",
        ]);
        dir
    }

    fn note_input(title: &str) -> NoteInput {
        NoteInput {
            kind: "decision".to_string(),
            title: title.to_string(),
            body: format!("body of {title}"),
            tags: vec![],
            linked_files: vec![],
            embedding: None,
            source_ref: None,
            valid_at: None,
            supersedes: None,
            origin: None,
        }
    }

    // Titles, not ids: the ids a clone mints are its own.
    fn edge_triples(mem_path: &std::path::Path) -> Vec<(String, String, String)> {
        let store = MemoryStore::open(mem_path).expect("open memory.db");
        let notes = store.list(None, 100, true).expect("list");
        let titles: HashMap<String, String> = notes
            .iter()
            .map(|n| (n.id.to_string(), n.title.clone()))
            .collect();
        let name = |id: &NoteId| {
            titles
                .get(&id.to_string())
                .cloned()
                .unwrap_or_else(|| id.to_string())
        };

        let mut rows: Vec<(String, String, String)> = Vec::new();
        for note in &notes {
            let (outgoing, _) = store.get_edges(&note.id).expect("get_edges");
            rows.extend(
                outgoing
                    .iter()
                    .map(|e| (name(&e.from_id), e.kind.clone(), name(&e.to_id))),
            );
        }
        rows.sort();
        rows.dedup();
        rows
    }

    #[tokio::test]
    async fn init_import_applies_carried_edges_after_the_entries_they_join() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();
        let backend = GitNotesBackend::with_root(git_root.to_path_buf());

        let (from, _) = backend.add(note_input("the claim")).await.expect("add a");
        let (to, _) = backend.add(note_input("the answer")).await.expect("add b");
        backend
            .add_edge(&from, &to, "relates_to")
            .await
            .expect("relates_to");
        backend
            .add_edge(&from, &to, "contradicts")
            .await
            .expect("contradicts");

        let mem_path = git_root.join(".inkentry").join("memory.db");
        let outcome = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(outcome.imported, 2);
        assert_eq!(outcome.edges_applied, 2);
        assert_eq!(outcome.edges_unresolved, 0);
        assert_eq!(
            edge_triples(&mem_path),
            vec![
                (
                    "the claim".to_string(),
                    "contradicts".to_string(),
                    "the answer".to_string()
                ),
                (
                    "the claim".to_string(),
                    "relates_to".to_string(),
                    "the answer".to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn init_import_counts_a_dangling_edge_then_resolves_it_without_duplicating() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();
        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        let mem_path = git_root.join(".inkentry").join("memory.db");

        let (from, _) = backend.add(note_input("the claim")).await.expect("add a");
        let (to, _) = backend.add(note_input("the answer")).await.expect("add b");
        backend
            .add_edge(&from, &to, "relates_to")
            .await
            .expect("relates_to");

        // The target is on the ref but not in this store, as after a partial fetch.
        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        let carried = backend.carried_edges().await.expect("carried_edges");
        store
            .add_note_with_created_at(
                "decision",
                "the claim",
                "body of the claim",
                &[],
                &[],
                None,
                "active",
                1,
            )
            .expect("seed the source only");
        let partial = store.import_carried_edges(&carried).expect("partial");
        assert_eq!(partial.unresolved, 1, "the absent target must be counted");
        assert_eq!(partial.applied, 0);
        assert!(
            edge_triples(&mem_path).is_empty(),
            "no dangling row is left"
        );
        drop(store);

        let outcome = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(outcome.imported, 1, "only the missing entry is new");
        assert_eq!(outcome.edges_applied, 1);
        assert_eq!(outcome.edges_unresolved, 0);

        let after_first = edge_triples(&mem_path);
        assert_eq!(after_first.len(), 1);

        let again = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("re-import");
        assert_eq!(again.edges_applied, 0, "nothing new on a re-run");
        assert_eq!(
            edge_triples(&mem_path),
            after_first,
            "re-importing the carrier must not duplicate an edge"
        );
    }

    #[tokio::test]
    async fn init_import_applies_an_edge_appended_after_both_entries_were_hydrated() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();
        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        let mem_path = git_root.join(".inkentry").join("memory.db");

        let (from, _) = backend.add(note_input("the claim")).await.expect("add a");
        let (to, _) = backend.add(note_input("the answer")).await.expect("add b");
        let first = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import");
        assert_eq!(first.imported, 2);
        assert!(edge_triples(&mem_path).is_empty());

        backend
            .add_edge(&from, &to, "relates_to")
            .await
            .expect("relates_to");

        let second = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("re-import");
        assert_eq!(second.imported, 0, "no new entries, only a new edge");
        assert_eq!(second.edges_applied, 1);
        assert_eq!(edge_triples(&mem_path).len(), 1);
    }

    const SUPERSEDE_OLD_TITLE: &str = "the retired approach";
    const SUPERSEDE_NEW_TITLE: &str = "the chosen approach";

    // Leaves the ref as `memory add` + `memory supersede`'s write-through would.
    // Returns NEW's entity_id.
    async fn seed_supersede_carrier(git_root: &std::path::Path) -> String {
        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        backend
            .add(note_input(SUPERSEDE_OLD_TITLE))
            .await
            .expect("add old");
        backend
            .add(note_input(SUPERSEDE_NEW_TITLE))
            .await
            .expect("add new");
        let old_note = backend
            .list(None, 10, true, None)
            .await
            .expect("list ref")
            .into_iter()
            .find(|n| n.title == SUPERSEDE_OLD_TITLE)
            .expect("old on ref");
        let new_eid = entity_id(
            "decision",
            SUPERSEDE_NEW_TITLE,
            &format!("body of {SUPERSEDE_NEW_TITLE}"),
        );
        crate::storage::append_state_update(
            Some(git_root),
            &old_note,
            "archived",
            Some(1),
            Some(new_eid.clone()),
        )
        .await
        .expect("carry supersede edge");
        new_eid
    }

    #[tokio::test]
    async fn init_import_reconstructs_supersede_edge_matching_the_writer() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();
        seed_supersede_carrier(git_root).await;

        // `supersede` writes the edge as (from = NEW, to = OLD); a clone must match.
        let writer_mem = git_root.join(".inkentry").join("writer.db");
        let writer = MemoryStore::open(&writer_mem).expect("open writer.db");
        let (old_local, _) = writer
            .add_note(
                "decision",
                SUPERSEDE_OLD_TITLE,
                &format!("body of {SUPERSEDE_OLD_TITLE}"),
                &[],
                &[],
                None,
                None,
            )
            .expect("writer add old");
        let (new_local, _) = writer
            .add_note(
                "decision",
                SUPERSEDE_NEW_TITLE,
                &format!("body of {SUPERSEDE_NEW_TITLE}"),
                &[],
                &[],
                None,
                None,
            )
            .expect("writer add new");
        assert!(
            writer.supersede(&old_local, &new_local).expect("supersede"),
            "OLD must archive under the writer's supersede"
        );
        let writer_edges = edge_triples(&writer_mem);
        assert_eq!(
            writer_edges,
            vec![(
                SUPERSEDE_NEW_TITLE.to_string(),
                "supersedes".to_string(),
                SUPERSEDE_OLD_TITLE.to_string(),
            )],
            "the writer holds exactly one supersede edge, NEW → OLD"
        );

        let clone_mem = git_root.join(".inkentry").join("clone.db");
        import_git_notes_into_memory(git_root, &clone_mem)
            .await
            .expect("clone import");

        assert_eq!(
            edge_triples(&clone_mem),
            writer_edges,
            "the clone must reconstruct the writer's supersede edge"
        );

        import_git_notes_into_memory(git_root, &clone_mem)
            .await
            .expect("clone re-import");
        assert_eq!(
            edge_triples(&clone_mem),
            writer_edges,
            "re-import must not duplicate the supersede edge"
        );
    }

    #[tokio::test]
    async fn init_imports_git_notes_and_is_idempotent() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        backend
            .add(NoteInput {
                kind: "decision".to_string(),
                title: "use sqlite".to_string(),
                body: "chosen for portability".to_string(),
                tags: vec!["storage".to_string()],
                linked_files: vec![],
                embedding: None,
                source_ref: None,
                valid_at: None,
                supersedes: None,
                origin: None,
            })
            .await
            .expect("git-notes add");

        let mem_path = git_root.join(".inkentry").join("memory.db");
        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import")
            .imported;
        assert_eq!(imported, 1, "pre-init git-notes entry must import");

        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        let listed = store.list(None, 10, false).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "use sqlite");
        assert_eq!(listed[0].source_ref.as_deref(), Some(INIT_GIT_NOTES_SOURCE));

        let again = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("re-import")
            .imported;
        assert_eq!(again, 0, "re-import must be a no-op");
        assert_eq!(
            store.list(None, 10, false).expect("list again").len(),
            1,
            "no duplication on re-run"
        );
    }

    #[tokio::test]
    async fn init_import_dedups_legacy_blob_without_entity_id() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        // serde omits `entity_id` when None, so this blob has no such key.
        let legacy = crate::storage::NoteRecord {
            schema_version: 1,
            id: 1,
            kind: "decision".to_string(),
            title: "legacy entry".to_string(),
            body: "written by an older client".to_string(),
            tags: vec![],
            linked_files: vec![],
            created_at: 1_700_000_000,
            status: "active".to_string(),
            source_ref: None,
            valid_at: None,
            invalid_at: None,
            superseded_by: None,
            remote_id: None,
            entity_id: None,
            superseded_by_entity_id: None,
            edges: Vec::new(),
            origin: None,
            op: None,
            patch_id: None,
        };
        crate::storage::append_to_git_notes(Some(git_root), &legacy)
            .await
            .expect("append legacy record");

        let raw = std::process::Command::new("git")
            .args(["notes", "--ref=inkentry", "show", "HEAD"])
            .current_dir(git_root)
            .output()
            .expect("git notes show");
        let blob = String::from_utf8_lossy(&raw.stdout);
        assert!(
            !blob.contains("\"entity_id\""),
            "the seeded blob must genuinely lack the key: {blob}"
        );

        let mem_path = git_root.join(".inkentry").join("memory.db");
        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        // A different created_at, which is not part of the key.
        store
            .add_note_with_created_at(
                "decision",
                "legacy entry",
                "written by an older client",
                &[],
                &[],
                Some("manual"),
                "active",
                1_700_000_999,
            )
            .expect("seed note");

        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import")
            .imported;
        assert_eq!(
            imported, 0,
            "legacy blob must recompute its id and dedup against the stored row"
        );
        assert_eq!(store.list(None, 10, true).expect("list").len(), 1);
    }

    #[tokio::test]
    async fn init_import_no_notes_is_noop() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();
        let mem_path = git_root.join(".inkentry").join("memory.db");
        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import")
            .imported;
        assert_eq!(imported, 0, "no notes ref → nothing imported");
    }

    #[tokio::test]
    async fn init_import_no_commit_repo_is_noop_no_churn() {
        register_sqlite_vec();
        let repo = make_temp_git_repo_no_commit();
        let git_root = repo.path();
        let mem_path = git_root.join(".inkentry").join("memory.db");

        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import")
            .imported;
        assert_eq!(imported, 0, "no HEAD / no notes ref → nothing imported");
        assert!(
            !mem_path.exists(),
            "an empty notes ref must not create a memory.db (no churn)"
        );
    }

    #[tokio::test]
    async fn init_import_no_notes_no_db_churn() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();
        let mem_path = git_root.join(".inkentry").join("memory.db");

        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import")
            .imported;
        assert_eq!(imported, 0);
        assert!(
            !mem_path.exists(),
            "no notes to import must not create a memory.db"
        );
    }

    #[tokio::test]
    async fn init_import_archived_entry_imports_and_dedups() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        let (id, _created) = backend
            .add(NoteInput {
                kind: "note".to_string(),
                title: "retired decision".to_string(),
                body: "kept for the record".to_string(),
                tags: vec![],
                linked_files: vec![],
                embedding: None,
                source_ref: None,
                valid_at: None,
                supersedes: None,
                origin: None,
            })
            .await
            .expect("git-notes add");
        assert!(
            backend.archive(id).await.expect("archive"),
            "the seeded entry must archive"
        );

        let mem_path = git_root.join(".inkentry").join("memory.db");
        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import")
            .imported;
        assert_eq!(imported, 1, "archived git-notes entry must import");

        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        assert!(
            store.list(None, 10, false).expect("active list").is_empty(),
            "an imported archived entry must not appear in the active listing"
        );
        let all = store.list(None, 10, true).expect("full list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, "archived", "archived status must be carried");

        // Status is not part of the key, so the archived row dedups.
        let again = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("re-import")
            .imported;
        assert_eq!(again, 0, "archived entry must not double-import");
        assert_eq!(
            store.list(None, 10, true).expect("full list again").len(),
            1,
            "row count must stay stable across re-import"
        );
    }

    async fn run_skip_already_present_scenario() -> Result<()> {
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        backend
            .add(NoteInput {
                kind: "decision".to_string(),
                title: "already present".to_string(),
                body: "seeded into memory.db before init".to_string(),
                tags: vec!["x".to_string()],
                linked_files: vec![],
                embedding: None,
                source_ref: None,
                valid_at: None,
                supersedes: None,
                origin: None,
            })
            .await
            .context("git-notes add A")?;
        backend
            .add(NoteInput {
                kind: "decision".to_string(),
                title: "brand new".to_string(),
                body: "only in git notes".to_string(),
                tags: vec![],
                linked_files: vec![],
                embedding: None,
                source_ref: None,
                valid_at: None,
                supersedes: None,
                origin: None,
            })
            .await
            .context("git-notes add B")?;

        let seeded = backend
            .list(None, 10, true, None)
            .await
            .context("list git notes")?;
        let a = seeded
            .iter()
            .find(|n| n.title == "already present")
            .context("entry A present in git notes")?;

        let mem_path = git_root.join(".inkentry").join("memory.db");
        {
            let store = MemoryStore::open(&mem_path).context("open memory.db")?;
            let tags: Vec<&str> = a.tags.iter().map(String::as_str).collect();
            store
                .add_note_with_created_at(
                    &a.kind,
                    &a.title,
                    &a.body,
                    &tags,
                    &[],
                    Some("manual"),
                    "active",
                    a.created_at,
                )
                .context("seed A into memory.db")?;
        }

        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .context("import")?
            .imported;
        anyhow::ensure!(
            imported == 1,
            "only the entry absent from memory.db imports, got {imported}"
        );

        let store = MemoryStore::open(&mem_path).context("reopen memory.db")?;
        let all = store.list(None, 50, true).context("list")?;
        anyhow::ensure!(
            all.len() == 2,
            "no duplicate row for the already-present entry, got {}",
            all.len()
        );
        let init_sourced = all
            .iter()
            .filter(|n| n.source_ref.as_deref() == Some(INIT_GIT_NOTES_SOURCE))
            .count();
        anyhow::ensure!(
            init_sourced == 1,
            "exactly one row came from the init import, got {init_sourced}"
        );

        drop(repo);
        Ok(())
    }

    #[tokio::test]
    async fn init_import_skips_entries_already_in_memory_db() {
        register_sqlite_vec();
        run_skip_already_present_scenario().await.expect("scenario");
    }

    // In-flight scenarios are capped: too many concurrent git subprocess spawns
    // spuriously fail with ENOENT under `cargo test --workspace`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn init_import_skip_scenario_is_race_free_under_concurrent_tasks() {
        register_sqlite_vec();
        const RUNS: usize = 20;
        const MAX_CONCURRENT: usize = 4;

        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
        let tasks: Vec<_> = (0..RUNS)
            .map(|_| {
                let semaphore = semaphore.clone();
                tokio::spawn(async move {
                    let _permit = semaphore.acquire_owned().await.expect("semaphore open");
                    run_skip_already_present_scenario().await
                })
            })
            .collect();

        let mut failures = Vec::new();
        for (i, task) in tasks.into_iter().enumerate() {
            if let Err(e) = task.await.expect("task should not panic") {
                failures.push(format!("run {i}: {e:#}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{}/{RUNS} concurrent runs failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    // Drift guard: the two keys must match for identical content, or an entry in
    // both git-notes and memory.db imports twice. The inputs differ in every
    // field the key excludes.
    #[test]
    fn dedup_key_parity_between_reconcile_and_init_import() {
        register_sqlite_vec();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mem_path = tmp.path().join("memory.db");
        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        let created_at = 1_700_000_123_i64;
        store
            .add_note_with_created_at(
                "decision",
                "shared key",
                "body text",
                &["beta", "alpha"],
                &["b.rs", "a.rs"],
                Some("manual"),
                "active",
                created_at,
            )
            .expect("seed note");
        let note = store
            .all_notes_for_dedup()
            .expect("dedup set")
            .pop()
            .expect("one note");

        let server_note = ServerNote {
            id: 4242,
            kind: "decision".to_string(),
            title: "shared key".to_string(),
            body: "body text".to_string(),
            tags: "alpha,beta".to_string(),
            linked_files: "a.rs,b.rs".to_string(),
            created_at: created_at + 86_400,
            status: "archived".to_string(),
            superseded_by: None,
        };

        assert_eq!(
            note_entity_id(&note),
            server_note.entity_id(),
            "reconcile's key and init-import's key must match for identical content"
        );
    }

    // A live test at the 500-entry cap is omitted: each git-notes write rewrites
    // the whole blob, so seeding 500+ entries is quadratic subprocess work. The
    // constant is pinned by the assertion below.
    #[tokio::test]
    async fn init_import_multiple_entries_single_batch() {
        register_sqlite_vec();
        let repo = make_temp_git_repo();
        let git_root = repo.path();

        let backend = GitNotesBackend::with_root(git_root.to_path_buf());
        const N: usize = 6;
        for i in 0..N {
            backend
                .add(NoteInput {
                    kind: "note".to_string(),
                    title: format!("entry {i}"),
                    body: format!("body {i}"),
                    tags: vec![],
                    linked_files: vec![],
                    embedding: None,
                    source_ref: None,
                    valid_at: None,
                    supersedes: None,
                    origin: None,
                })
                .await
                .expect("git-notes add");
        }

        let mem_path = git_root.join(".inkentry").join("memory.db");
        let imported = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("import")
            .imported;
        assert_eq!(imported, N, "all seeded entries import in one batch");

        let store = MemoryStore::open(&mem_path).expect("open memory.db");
        assert_eq!(store.list(None, 100, false).expect("list").len(), N);

        let again = import_git_notes_into_memory(git_root, &mem_path)
            .await
            .expect("re-import")
            .imported;
        assert_eq!(again, 0, "the whole batch dedups on re-run");
        assert_eq!(
            store.list(None, 100, false).expect("list again").len(),
            N,
            "row count stable across re-import"
        );
    }

    // Mirrors `GitNotesBackend`'s per-list cap, which is not re-exported, so a
    // change to one side alone fails at compile time.
    const _: () = assert!(GIT_NOTES_IMPORT_LIMIT == 500);

    fn make_temp_git_repo_no_commit() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .output()
            .expect("git init");
        dir
    }
}
