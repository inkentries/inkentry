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

    let summary = {
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
    if crate::cli::cmd::memory::reindex::memory_reindex(reindex, mem_path, cfg, None)
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
