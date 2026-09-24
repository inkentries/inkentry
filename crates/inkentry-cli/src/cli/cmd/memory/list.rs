use anyhow::Result;

use super::MemoryListArgs;
use super::{parse_as_of, print_note_summary};
use crate::{
    config::Config,
    storage::{NoteId, open_memory_backend},
};

pub(super) async fn memory_list(
    args: MemoryListArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
    pre_init_notes: bool,
) -> Result<()> {
    let started = std::time::Instant::now();
    // Read from git notes when it's the explicit backend (`--backend git-notes`)
    // or the ADR-068 D3 pre-init carrier: `mem_path` is a placeholder in both, so
    // skip the SQLite-oriented nudge and cross-project pass (they'd open the
    // local/global SQLite store) and route the read to `refs/notes/inkentry`.
    let git_notes = pre_init_notes || backend_override == Some("git-notes");
    let effective_override = if git_notes {
        Some("git-notes")
    } else {
        backend_override
    };

    // A teammate's `git fetch` lands their notes on a tracking ref that nothing
    // else merges into `memory.db`, so without this they stay invisible on the
    // default read path (ADR-077 D1).
    super::reconcile::refresh_read_path_from_git_notes(cfg, mem_path, effective_override).await;

    // Discovery nudge: warn once when unimported server.db notes exist.
    if !git_notes {
        super::reconcile::maybe_emit_nudge(mem_path, cfg);
        super::outbox::poll_and_apply(cfg, mem_path).await;
    }

    let as_of = parse_as_of(args.as_of.as_deref())?;

    // `--tag`/`--file` (ADR-101 D4) are exact filters backed by the
    // `note_tags`/`note_files` indexes, which only the local sqlite store
    // has. They bypass the `MemoryBackend` trait (which has no such method,
    // and would need one on every backend for a filter two of the three
    // cannot serve) and the cross-project/`--source-ref` combinations below,
    // which is why they are handled up front and return early.
    if args.tag.is_some() || args.file.is_some() {
        anyhow::ensure!(
            !git_notes,
            "--tag/--file require the sqlite backend; re-run without --backend git-notes"
        );
        let store = crate::storage::MemoryStore::open(mem_path)?;
        let notes = store.list_filtered_ext(
            args.kind.as_deref(),
            None,
            args.tag.as_deref(),
            args.file.as_deref(),
            args.limit,
            args.archived,
            as_of,
        )?;
        let result = print_notes(&notes, &args.format);
        record_list_event(
            cfg,
            mem_path,
            effective_override,
            &notes,
            started,
            result.is_ok(),
        );
        return result;
    }

    let backend = open_memory_backend(cfg, mem_path, effective_override).await?;
    let mut notes = if let Some(ref sha_prefix) = args.source_ref {
        // (1) Harvest-provenance matches: entries whose `source_ref` COLUMN
        // records this commit (harvested entries, ADR-062). On the git-notes
        // backend this instead returns the note-anchored entries directly, so
        // that path is already complete here.
        let mut matches = backend
            .list_by_source_ref(sha_prefix, args.limit, args.archived, as_of)
            .await?;
        // (2) Note-anchored matches: a `memory add` entry records which commit
        // it belongs to only as the git-notes attachment; its `source_ref`
        // COLUMN stays NULL, so (1) can never surface it (the reported bug).
        // Resolve the ids anchored to the commit from the notes ref, then read
        // the authoritative local rows back so the listing keeps this store's
        // own ids and status. SQLite-primary only: the git-notes backend covers
        // its own path in (1), and a remote backend has no local notes ref.
        if backend.backend_kind() == "sqlite" {
            augment_with_note_anchored(
                &mut matches,
                mem_path,
                sha_prefix,
                args.limit,
                args.archived,
                as_of,
            )
            .await;
        }
        matches
    } else {
        backend
            .list(args.kind.as_deref(), args.limit, args.archived, as_of)
            .await?
    };

    // Cross-project dep pass (ADR-003): append locked/cross-project decisions
    // and requirements from linked projects unless --local-only is set.
    // The dep pass is skipped when --source-ref is given (commit-specific queries
    // are inherently local) or when --archived is set (archived entries are
    // project-local housekeeping noise, not cross-cutting signals).
    if !args.local_only && args.source_ref.is_none() && !args.archived && !git_notes {
        let index_db_path = crate::config::resolve_db(None, &cfg.db_path);
        let mut seen: std::collections::HashSet<(String, NoteId)> = Default::default();
        // Seed seen set from local results so local entries don't collide with
        // same-id entries from a dep that happens to share a local path (unlikely
        // but defensive). Local notes have no root_path key, so we use "".
        for n in &notes {
            seen.insert((String::new(), n.id.clone()));
        }
        let dep_notes =
            super::cross_project::collect_dep_cross_cutting(&index_db_path, &mut seen).await;
        // Filter dep notes to the requested kind (if --kind was specified).
        let dep_notes: Vec<_> = if let Some(ref k) = args.kind {
            dep_notes.into_iter().filter(|n| &n.kind == k).collect()
        } else {
            dep_notes
        };
        notes.extend(dep_notes);
    }

    let result = print_notes(&notes, &args.format);
    record_list_event(
        cfg,
        mem_path,
        effective_override,
        &notes,
        started,
        result.is_ok(),
    );
    result
}

/// Shared best-effort event recording (ADR-098 D5) for both `memory list`
/// paths above.
fn record_list_event(
    cfg: &Config,
    mem_path: &std::path::Path,
    backend_override: Option<&str>,
    notes: &[crate::storage::memory::Note],
    started: std::time::Instant,
    ok: bool,
) {
    let returned_ids: Vec<String> = notes.iter().map(|n| n.entity_id.clone()).collect();
    super::super::events::record(
        cfg,
        mem_path,
        backend_override,
        "memory.list",
        None,
        Some(returned_ids.len() as i64),
        &returned_ids,
        None,
        started,
        ok,
    );
}

/// Shared output for every `memory list` path (the normal query and the
/// `--tag`/`--file` sqlite-direct path above).
fn print_notes(notes: &[crate::storage::memory::Note], format: &str) -> Result<()> {
    if notes.is_empty() {
        println!("No memory entries found.");
        return Ok(());
    }

    match crate::utils::effective_format(format) {
        "json" => println!("{}", serde_json::to_string_pretty(notes)?),
        "jsonl" => {
            for n in notes {
                println!("{}", serde_json::to_string(n)?);
            }
        }
        _ => {
            for n in notes {
                print_note_summary(n);
            }
        }
    }
    Ok(())
}

/// Append the entries anchored (via git notes) to the `--source-ref` commit to
/// `matches`, deduped by `entity_id`.
///
/// The commit anchor of a `memory add` entry lives only in the enclosing repo's
/// notes ref (commit → note object); it is never written into the SQLite
/// `source_ref` column, so the column query in `memory_list` cannot find it.
/// This reads the ids anchored to the commit off the notes ref, then reads the
/// authoritative local rows back so the listing keeps this store's own ids and
/// status.
///
/// Best-effort by design: outside a git repo, with no `refs/notes/inkentry`, or
/// on any git failure there is simply nothing to add and the column matches in
/// `matches` stand. `mem_path.parent()` is the same root the write-through
/// carrier anchors against (see `memory add`), so reads and writes agree on
/// which repo owns the notes.
async fn augment_with_note_anchored(
    matches: &mut Vec<crate::storage::memory::Note>,
    mem_path: &std::path::Path,
    sha_prefix: &str,
    limit: usize,
    include_archived: bool,
    as_of: Option<i64>,
) {
    use crate::storage::{GitNotesBackend, MemoryStore, note_entity_id};

    let Some(project_root) = mem_path.parent() else {
        return;
    };
    let anchored_ids = match GitNotesBackend::with_root(project_root.to_path_buf())
        .entity_ids_anchored_to(sha_prefix)
        .await
    {
        Ok(ids) => ids,
        Err(_) => return,
    };
    if anchored_ids.is_empty() {
        return;
    }

    let Ok(store) = MemoryStore::open(mem_path) else {
        return;
    };
    let anchored = match store.list_by_entity_ids(&anchored_ids, limit, include_archived, as_of) {
        Ok(notes) => notes,
        Err(_) => return,
    };
    if anchored.is_empty() {
        return;
    }

    // Dedup by entity_id: an entry that is both harvested (column match) and
    // note-anchored must appear once.
    let mut seen: std::collections::HashSet<String> = matches.iter().map(note_entity_id).collect();
    for n in anchored {
        if seen.insert(note_entity_id(&n)) {
            matches.push(n);
        }
    }
    // Restore the newest-first order and re-cap so the union still honours the
    // limit the single-source query would have.
    matches.sort_by_key(|n| std::cmp::Reverse(n.created_at));
    matches.truncate(limit.min(500));
}
