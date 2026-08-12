use anyhow::{Context, Result};
use clap::Args;
use std::path::PathBuf;

use crate::config::Config;
use crate::dump;
use crate::registry::Registry;
use crate::storage::MemoryStore;

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Path to the dump file to import
    pub path: PathBuf,

    /// Path to the memory database (overrides auto-detect)
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Import without bringing the entries into semantic search. The finishing
    /// command is reported either way.
    #[arg(long)]
    pub no_embed: bool,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

/// Import a portable dump into this project's stores.
///
/// Three properties shape the order of what follows:
///
/// * The dump is read and verified **whole** before anything is written. A
///   truncated or altered dump is refused outright; there is no partial import.
/// * The write runs in one transaction with **no embedding inside it**. Vectors
///   are not carried in a dump, and a network call per row under a write lock
///   is not a shape to repeat.
/// * Embedding is attempted afterwards, and its absence is **reported rather
///   than swallowed**. An imported store that is never embedded still answers
///   searches — the default mode is hybrid, so full-text carries it — which is
///   worse than an empty result, because nothing signals the problem.
pub async fn import(args: ImportArgs, cfg: Config) -> Result<()> {
    cfg.validate()?;
    refuse_when_memory_is_not_local(&cfg)?;
    let json = crate::utils::effective_format(&args.format) == "json";

    let bytes = std::fs::read(&args.path)
        .with_context(|| format!("reading dump {}", args.path.display()))?;
    let parsed = dump::read(&bytes).with_context(|| {
        format!(
            "{} is not a dump this build can import",
            args.path.display()
        )
    })?;

    let (mem_path, _) =
        crate::cli::cmd::memory::resolve_store_path(args.db.clone(), false, &cfg, None)
            .await
            .context("resolving where this project's memory lives")?;

    let outcome = {
        let store = MemoryStore::open(&mem_path)
            .with_context(|| format!("opening memory store at {}", mem_path.display()))?;
        let registry = Registry::open().ok();
        let index_db = cfg.db_path.clone();
        let targets = dump::ImportTargets {
            memory: &store,
            registry: registry.as_ref(),
            index_db: Some(index_db.as_path()),
        };
        dump::apply(&parsed, &targets)?
    };
    let mut summary = outcome.summary;
    let carried = carry_to_git_notes(&cfg, &mem_path, &outcome.carrier_records).await;
    if let Some(carried) = &carried {
        summary.memory_entries_carried = carried.written;
        summary.memory_entries_already_carried = carried.already_carried;
    }

    if json {
        println!("{}", serde_json::to_string(&summary)?);
    } else {
        println!(
            "Imported {} memory entr{}, {} relationship{}, {} project{}.",
            summary.memory_entries,
            if summary.memory_entries == 1 {
                "y"
            } else {
                "ies"
            },
            summary.memory_edges,
            if summary.memory_edges == 1 { "" } else { "s" },
            summary.projects,
            if summary.projects == 1 { "" } else { "s" },
        );
        report_records_that_did_not_become_rows(&summary);
        report_what_travels(carried.as_ref());
    }

    finish_embeddings(
        &args,
        &mem_path,
        &cfg,
        summary.entries_needing_embedding,
        json,
    )
    .await;
    Ok(())
}

/// Refuse before reading the file when this project's memory does not live in
/// a local SQLite store.
///
/// The import writes through `MemoryStore` directly. Under `cloud_first` with a
/// `server_url`, `open_memory_backend` makes that server the store of record,
/// so a local write would land in a file every memory command reads past —
/// success reported, data gone, on a move designed to be made once. The
/// condition mirrors `open_memory_backend`'s `route_remote` exactly:
/// `cloud_first` with no `server_url` has nothing to route to and resolves
/// local, so it imports normally.
///
/// Importing into the server instead is not an option this can take: the remote
/// backend adds entries through `add`, which mints its own identity and carries
/// neither `entity_id` nor `created_at` verbatim, and it has no transaction to
/// roll back — the two properties the dump format and this command exist to
/// preserve. A refusal that names the recovery path keeps both.
fn refuse_when_memory_is_not_local(cfg: &Config) -> Result<()> {
    use crate::config::SyncMode;

    if cfg.resolve_mode() != SyncMode::CloudFirst {
        return Ok(());
    }
    let Some(url) = cfg.server_url.as_deref() else {
        return Ok(());
    };
    anyhow::bail!(
        "this project's memory lives on {url} (mode = cloud_first), and 'inkentry import' \
         writes to the local memory store. Importing here would leave every entry in a file \
         this project never reads. Import into the local store first — re-run with \
         INKENTRY_MODE=local_first — then 'inkentry sync' to carry it up to {url}."
    );
}

/// Append the imported entries to `refs/notes/inkentry`, so they clone with the
/// repository like every other memory entry.
///
/// Runs **after** the import transaction commits, and is best-effort, exactly
/// as `memory add`'s write-through is: the local store is the store of record
/// and already holds the entries, so a failed carry is a warning rather than a
/// reason to fail a crossing the user cannot easily repeat. Unlike `memory
/// add`'s pre-`init` case there is no variant where the carrier is the sole
/// store — an import always has a `memory.db` to write to first.
///
/// Returns `None` when there is nothing to carry to: `store_in_git_notes` is
/// off, or this store does not sit inside a git repository (`--db` pointed
/// outside one, or the project is not versioned). Neither is a failure, and
/// neither has anything to report.
///
/// The repo is resolved from `mem_path`, never the process CWD: a `--db` naming
/// another project's store must carry to that project's repo, not to whichever
/// one the user happens to be standing in.
async fn carry_to_git_notes(
    cfg: &Config,
    mem_path: &std::path::Path,
    records: &[inkentry_core::storage::NoteRecord],
) -> Option<inkentry_core::storage::BatchAppendOutcome> {
    use inkentry_core::storage::{NotesRefs, append_new_to_git_notes};

    if !cfg.store_in_git_notes || records.is_empty() {
        return None;
    }
    let git_root = NotesRefs::discover(mem_path.parent())?
        .workdir()?
        .to_path_buf();

    match append_new_to_git_notes(Some(&git_root), records).await {
        Ok(outcome) => {
            // Both warnings go to stderr rather than joining the counts on
            // stdout, so they survive `--format json`, where stdout carries
            // exactly one document.
            //
            // Visible without RUST_LOG: an unserialized write can lose a
            // concurrent entry, and this is the only channel that reaches the
            // user (ADR-069 D8).
            if let Some(degradation) = &outcome.lock_degradation {
                eprintln!("Warning: {degradation}");
            }
            if outcome.rewrite_ref == inkentry_core::storage::RewriteRefStatus::Failed {
                eprintln!(
                    "Warning: could not set git notes.rewriteRef, so memory may not survive \
                     `git commit --amend` or `git rebase`. Set it with: \
                     git config --add notes.rewriteRef refs/notes/inkentry"
                );
            }
            Some(outcome)
        }
        // Visible without RUST_LOG: a swallowed carry failure is how imported
        // memory silently stops traveling with the repo, which is the defect
        // this write exists to close.
        Err(e) => {
            eprintln!(
                "Warning: entries were imported into the local store, but the git-notes \
                 carry failed, so they will not travel with the repo: {e:#}"
            );
            None
        }
    }
}

/// Say what will and will not clone with the repository.
///
/// The already-carried count is not noise: it is the answer for someone
/// re-importing a dump that came off this repo's own notes ref, who would
/// otherwise read "carried 0" as a failure.
fn report_what_travels(carried: Option<&inkentry_core::storage::BatchAppendOutcome>) {
    let Some(carried) = carried else { return };
    if carried.written > 0 {
        println!(
            "Carried {} entr{} into git notes, so {} travel with the repository.",
            carried.written,
            if carried.written == 1 { "y" } else { "ies" },
            if carried.written == 1 { "it" } else { "they" },
        );
    }
    if carried.already_carried > 0 {
        println!(
            "{} w{} already in this repository's git notes and {} written again.",
            carried.already_carried,
            if carried.already_carried == 1 {
                "as"
            } else {
                "ere"
            },
            if carried.already_carried == 1 {
                "was not"
            } else {
                "were not"
            },
        );
    }
    // Announced only by the call that set it, so a repo says this once; the
    // failure case is a warning and went to stderr with the others.
    if carried.rewrite_ref == inkentry_core::storage::RewriteRefStatus::Configured {
        println!(
            "Configured git notes.rewriteRef in this repo, so memory now survives \
             `git commit --amend` and `git rebase`."
        );
    }
}

/// Say what happened to the records that did not become a row.
///
/// The count above is rows, not records, and on a one-way move the difference
/// is the number a user would otherwise have to go and count themselves. A
/// merge in particular loses the folded entry's own `source_ref`, `created_at`
/// and status, so it is the one outcome that must not pass in silence.
fn report_records_that_did_not_become_rows(summary: &inkentry_core::dump::ImportSummary) {
    if summary.memory_entries_merged > 0 {
        println!(
            "{} further entr{} shared an identity with one of them and {} folded in: \
             entries are identified by their content, so one identity is one entry.",
            summary.memory_entries_merged,
            if summary.memory_entries_merged == 1 {
                "y"
            } else {
                "ies"
            },
            if summary.memory_entries_merged == 1 {
                "was"
            } else {
                "were"
            },
        );
    }
    if summary.memory_entries_already_present > 0 {
        println!(
            "{} w{} already in this store and {} added again.",
            summary.memory_entries_already_present,
            if summary.memory_entries_already_present == 1 {
                "as"
            } else {
                "ere"
            },
            if summary.memory_entries_already_present == 1 {
                "was not"
            } else {
                "were not"
            },
        );
    }
}

/// Bring the imported entries into semantic search, using `memory reindex`'s
/// own pass rather than a second mechanism that could drift from it.
///
/// Never fatal. The import has already committed and is complete on its own
/// terms; an unreachable embedder is a reason to say what is left to do, not to
/// fail a crossing the user cannot easily repeat.
async fn finish_embeddings(
    args: &ImportArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    pending: usize,
    json: bool,
) {
    if pending == 0 {
        return;
    }
    if args.no_embed {
        report_pending(pending, json);
        return;
    }

    let reindex = crate::cli::cmd::memory::MemoryReindexArgs {
        force: false,
        // Matches `memory reindex`'s own default: active entries only.
        include_archived: false,
        dry_run: false,
        format: args.format.clone(),
    };
    // In json mode stdout carries exactly one document — the import summary
    // printed above — so the finishing pass does not print its own. Anything
    // it has to say about the part that is not done reaches the user through
    // `report_pending`, on stderr.
    let summary_output = if json {
        crate::cli::cmd::memory::reindex::Summary::Suppressed
    } else {
        crate::cli::cmd::memory::reindex::Summary::Printed
    };
    if crate::cli::cmd::memory::reindex::memory_reindex(
        reindex,
        mem_path,
        cfg,
        None,
        summary_output,
    )
    .await
    .is_err()
    {
        report_pending(pending, json);
        return;
    }

    // Reindex embeds what it can and reports its own totals, but a partial run
    // still leaves entries out of semantic search, so the count is re-read
    // rather than assumed to be zero.
    let still_pending = MemoryStore::open(mem_path)
        .and_then(|s| s.notes_missing_embeddings(false))
        .map(|v| v.len())
        .unwrap_or(pending);
    if still_pending > 0 {
        report_pending(still_pending, json);
    }
}

/// Said on stderr, unconditionally — not through `tracing`, which is invisible
/// without `RUST_LOG`. Semantic search degrades silently otherwise: the default
/// search mode is hybrid, so these entries still come back from the full-text
/// half and the store looks like it is working.
fn report_pending(pending: usize, json: bool) {
    if json {
        eprintln!(
            "{}",
            serde_json::json!({
                "warning": "entries_missing_embeddings",
                "count": pending,
                "run": "inkentry memory reindex",
            })
        );
        return;
    }
    eprintln!(
        "[inkentry] {pending} imported entr{} not in semantic search yet. \
         Full-text search already finds them, so results will look complete. \
         Run 'inkentry memory reindex' with a server running to finish.",
        if pending == 1 { "y is" } else { "ies are" }
    );
}
