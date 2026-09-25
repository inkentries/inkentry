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

// The write transaction embeds nothing (no network call per row under a write
// lock). Embedding runs afterwards and a shortfall is reported: hybrid search
// would otherwise quietly answer from full-text alone.
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
        if summary.memory_entries > 0 {
            println!(
                "The ids these entries show were minted on this machine; quote the entity id \
                 from `inkentry memory show` to name an entry anywhere else."
            );
        }
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

// Under `cloud_first` with a `server_url` the server is the store of record, so a
// local write would land in a file every memory command reads past. Mirrors
// `open_memory_backend`'s `route_remote`. Importing into the server is not an
// option: remote `add` mints its own identity and timestamps and has no
// transaction to roll back.
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

// Best-effort after the import commits: the local store already holds the
// entries, so a failed carry warns rather than failing an import that is hard
// to repeat. The repo is resolved from `mem_path`, not the CWD, so a `--db` for
// another project carries to that project's repo.
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
            // Warnings use stderr, not tracing, so they show without RUST_LOG
            // and leave `--format json` stdout as one document.
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
        Err(e) => {
            eprintln!(
                "Warning: entries were imported into the local store, but the git-notes \
                 carry failed, so they will not travel with the repo: {e:#}"
            );
            None
        }
    }
}

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
    // Without this, re-importing a dump from this repo's own notes ref reads as
    // "carried 0", which looks like a failure.
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
    // Only the call that set it announces, so a repo hears this once.
    if carried.rewrite_ref == inkentry_core::storage::RewriteRefStatus::Configured {
        println!(
            "Configured git notes.rewriteRef in this repo, so memory now survives \
             `git commit --amend` and `git rebase`."
        );
    }
}

// A merge drops the folded entry's own `source_ref`, `created_at` and status, so
// it must not pass in silence.
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

// Never fatal: the import has committed, so an unreachable embedder only
// reports what is left to do.
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
        include_archived: false,
        dry_run: false,
        format: args.format.clone(),
    };
    // In json mode stdout is already the import summary, so the pass prints none.
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

    // Reindex can be partial, so re-read the count rather than assume zero.
    let still_pending = MemoryStore::open(mem_path)
        .and_then(|s| s.notes_missing_embeddings(false))
        .map(|v| v.len())
        .unwrap_or(pending);
    if still_pending > 0 {
        report_pending(still_pending, json);
    }
}

// stderr, not tracing: these entries still appear in `memory list` and `context`,
// so the store looks populated while they miss semantic ranking.
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
         Text search is phrase-exact, so it is no fallback for them. \
         Run 'inkentry memory reindex' with a server running to finish.",
        if pending == 1 { "y is" } else { "ies are" }
    );
}
