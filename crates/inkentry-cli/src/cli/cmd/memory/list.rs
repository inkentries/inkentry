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
    // `mem_path` is a placeholder in both git-notes cases, so the SQLite-oriented
    // nudge and cross-project pass must be skipped.
    let git_notes = pre_init_notes || backend_override == Some("git-notes");
    let effective_override = if git_notes {
        Some("git-notes")
    } else {
        backend_override
    };

    // A teammate's fetched notes sit on a tracking ref nothing else merges into
    // `memory.db`, so they would stay invisible on the default read path.
    super::reconcile::refresh_read_path_from_git_notes(cfg, mem_path, effective_override).await;

    if !git_notes {
        super::reconcile::maybe_emit_nudge(mem_path, cfg);
        super::outbox::poll_and_apply(cfg, mem_path).await;
    }

    let as_of = parse_as_of(args.as_of.as_deref())?;

    // Only the local sqlite store has the `note_tags`/`note_files` indexes, and
    // `MemoryBackend` has no method for them, so these filters bypass it (and the
    // cross-project/`--source-ref` handling below).
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
        // Harvested entries match on the `source_ref` column; the git-notes
        // backend returns note-anchored entries here directly.
        let mut matches = backend
            .list_by_source_ref(sha_prefix, args.limit, args.archived, as_of)
            .await?;
        // A `memory add` entry's commit is recorded only as the git-notes
        // attachment, so the column query cannot find it. A remote backend has
        // no local notes ref.
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

    // Skipped for --source-ref (commit-specific, so inherently local) and
    // --archived (project-local housekeeping, not cross-cutting signal).
    if !args.local_only && args.source_ref.is_none() && !args.archived && !git_notes {
        let index_db_path = crate::config::resolve_db(None, &cfg.db_path);
        let mut seen: std::collections::HashSet<(String, NoteId)> = Default::default();
        // Local notes have no root_path key, hence "".
        for n in &notes {
            seen.insert((String::new(), n.id.clone()));
        }
        let dep_notes =
            super::cross_project::collect_dep_cross_cutting(&index_db_path, &mut seen).await;
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

// Best-effort: any git failure means nothing to add. Anchored ids are read back
// from the local store so the listing keeps its own ids and status.
// `mem_path.parent()` is the root `memory add` anchors against, so reads and
// writes agree on which repo owns the notes.
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

    // An entry both harvested and note-anchored must appear once.
    let mut seen: std::collections::HashSet<String> = matches.iter().map(note_entity_id).collect();
    for n in anchored {
        if seen.insert(note_entity_id(&n)) {
            matches.push(n);
        }
    }
    // Re-sort and re-cap so the union still honours `limit`.
    matches.sort_by_key(|n| std::cmp::Reverse(n.created_at));
    matches.truncate(limit.min(500));
}
