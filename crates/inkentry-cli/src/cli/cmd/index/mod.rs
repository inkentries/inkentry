use anyhow::{Context, Result};
use clap::Args;
use indicatif::MultiProgress;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct IndexArgs {
    /// Path to the codebase root to index
    pub path: PathBuf,

    /// Path to the SQLite database (overrides config)
    #[arg(short, long)]
    pub db: Option<PathBuf>,

    /// Cap on the embedding batch size: number of chunks sent per server
    /// request. The embed phase calibrates the actual per-request size from
    /// measured throughput (small batches on slow hardware, larger ones on
    /// fast hardware); this flag only sets the ceiling it may grow to. 0 (the
    /// default) leaves the ceiling at the server's own limit (256 chunks).
    #[arg(long, default_value = "0")]
    pub batch_size: usize,

    /// Force full re-index (ignore change detection)
    #[arg(long)]
    pub force: bool,

    /// Backfill token_count for all existing chunks and exit (useful for upgrading old indexes)
    #[arg(long)]
    pub recount: bool,

    /// Skip structural summary generation (the deterministic, offline pass that
    /// composes each chunk's `summary:` slot and, for title-less chunks, its
    /// tier-3 MMR slot)
    #[arg(long)]
    pub no_summaries: bool,

    /// Internal: run only the post-embed phases (tier-3 refinement, conventions).
    /// Used by the background process spawned after a large foreground index.
    #[arg(long = "_background-phases", hide = true, default_value_t = false)]
    pub background_phases: bool,

    /// Internal: skip parsing and run only the embed phase (plus phases 3-5)
    /// against the chunks already stored in the index. Used by the subprocess
    /// spawned for `--detach-embed`, which rebuilds the embed queue from the DB.
    #[arg(long = "_embed-phases", hide = true, default_value_t = false)]
    pub embed_phases: bool,

    /// Detach immediately: re-exec inkentry in the background and return.
    /// Useful in git hooks so the hook does not block the git process.
    #[arg(long, default_value_t = false)]
    pub detach: bool,

    /// Parse in the foreground, then hand the (usually long) embedding phase to
    /// a detached background process and return the prompt. Confirm completion
    /// later with `inkentry status` (it reports "embedding in progress" while the
    /// detached run has chunks left to embed).
    #[arg(long, default_value_t = false)]
    pub detach_embed: bool,

    // Filled in by `main` from the global `--config`; forwarded to detached children.
    #[arg(skip)]
    pub config_path: Option<PathBuf>,
}

use crate::{capability, config::Config, registry::Registry, storage::Database};

mod background_log;
mod continuation;
mod crash_test_hook;
mod embed_phase;
mod graph_pass;
mod parse_phase;
mod phases;
mod run_lock;
mod summaries;
mod tier3;
mod worktree;

pub async fn index(args: IndexArgs, cfg: Config) -> Result<()> {
    if args.detach {
        super::helpers::spawn_detached()?;
        return Ok(());
    }

    // A continuation child's streams are the background log; bracket its whole
    // run so lock re-acquire and index-open failures (before any phase) are logged.
    let Some(phase) = background_log::Phase::of(&args) else {
        return run_index(args, cfg).await;
    };
    background_log::activate();
    background_log::emit(format!("{phase} started (pid {})", std::process::id()));
    let outcome = run_index(args, cfg).await;
    match &outcome {
        Ok(()) => background_log::emit(format!("{phase} finished")),
        Err(e) => background_log::emit(format!("{phase} failed: {e:#}")),
    }
    outcome
}

async fn run_index(args: IndexArgs, cfg: Config) -> Result<()> {
    cfg.validate()?;

    crate::indexer::secrets::init();

    let project_root = worktree::resolve_main_worktree_root(&args.path);

    let db_path = args
        .db
        .clone()
        .unwrap_or_else(|| project_root.join(".inkentry").join("index.db"));

    // Concurrent writers corrupt index.db, so only one run may hold this.
    let inkentry_dir = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| project_root.join(".inkentry"));
    let mut run_lock = match run_lock::try_acquire(&inkentry_dir)? {
        run_lock::LockOutcome::Acquired(lock) => Some(lock),
        run_lock::LockOutcome::HeldByOther { holder_pid } => {
            let who = holder_pid
                .map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "another process".to_string());
            anyhow::bail!(
                "index already running ({who}) on this project, try again once it finishes"
            );
        }
    };

    let db = match Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            if args.force && db_path.exists() {
                tracing::warn!("corrupt index detected, deleting and rebuilding: {e}");
                std::fs::remove_file(&db_path)
                    .with_context(|| format!("removing corrupt index at {}", db_path.display()))?;
                Database::open(&db_path)?
            } else {
                return Err(e).with_context(|| {
                    format!(
                        "failed to open index at {}\n\
                         The database may be corrupt. Run with --force to delete it and rebuild from scratch:\n\
                         \n  inkentry index {} --force\n",
                        db_path.display(),
                        args.path.display(),
                    )
                });
            }
        }
    };
    super::helpers::announce_index_rebuild(&db);

    {
        let root_now = inkentry_core::utils::canonicalize(args.path.as_ref());
        let db_now = inkentry_core::utils::canonicalize(db_path.as_ref());
        if let Ok(reg) = Registry::open() {
            let _ = reg.register(&root_now, &db_now);
        }
    }

    if args.recount {
        let updated = db.backfill_token_counts()?;
        println!("Backfilled token counts for {updated} chunk(s).");
        return Ok(());
    }

    let root_canonical = inkentry_core::utils::canonicalize(args.path.as_ref());

    if args.background_phases {
        phases::run_background_phases(&args, &cfg, &db, &project_root, &root_canonical, &db_path)
            .await?;
        return Ok(());
    }

    if args.embed_phases {
        phases::run_embed_phases(&args, &cfg, &db, &project_root, &root_canonical, &db_path)
            .await?;
        return Ok(());
    }

    let mp = MultiProgress::new();

    let result = parse_phase::run_parse_phase(&root_canonical, &db, &args, &mp, &cfg)?;
    if result.removed > 0 {
        eprintln!("Removed {} stale file(s) from index.", result.removed);
    }

    // Retired after the walk rather than on file count, so a genuinely empty
    // tree reads as empty, not unrepaired.
    db.mark_reindexed()?;

    // Before the first embed so the queue is PageRank-ordered and each chunk's
    // first vector already carries its summary.
    phases::run_pre_embed_phases(&args, &db)?;

    // Not `get_tier`: local_first prefers the loopback embedder even when an
    // explicit server_url is set, and `get_tier` would probe that URL instead.
    let tier = capability::get_inference_tier(&cfg).await;

    // The parse-time queue predates PageRank and summaries, and misses pending re-embeds.
    let queue = parse_phase::missing_embedding_texts(&db)?;
    if queue.is_empty() {
        let stats = db.stats()?;
        println!(
            "Index: {} files, {} chunks, {} embeddings (nothing new to process)",
            stats.file_count, stats.chunk_count, stats.embedding_count
        );
        return Ok(());
    }

    // `index_embed` is advertised only once the embedder is ready; otherwise
    // skip with a notice rather than 503 mid-index.
    let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);

    // Gated on ready-or-loading, not `embed_ready`: the worker owns the
    // readiness wait, and a cold install arrives with the embedder still
    // loading, so gating on `embed_ready` would leave the index unembedded.
    if args.detach_embed && tier.is_server() && continuation::detach_embed_eligible(&tier) {
        let embed_log = continuation::background_log_path(&db_path);
        // Released so the child never interleaves writes with us; a third run
        // can still win the reacquire, which `wait_for_holder_pid` detects.
        drop(run_lock.take());
        crash_test_hook::pause_at("after_run_lock_drop", "embed");
        if let continuation::EmbedSpawn::Detached {
            log_in_use,
            child_pid,
        } = continuation::spawn_embed_subprocess(&args, embed_log.as_deref())?
        {
            let stats = db.stats()?;
            let pending = stats.pending_embed_count();
            if run_lock::wait_for_holder_pid(
                &inkentry_dir,
                child_pid,
                continuation::HANDOFF_CONFIRM_TIMEOUT,
                continuation::HANDOFF_POLL_INTERVAL,
            ) {
                println!(
                    "Index: {} files, {} chunks. Embedding {} chunk(s) in the background\u{2026}",
                    stats.file_count, stats.chunk_count, pending,
                );
                if !embed_ready {
                    println!("The embedder is still loading; the background worker waits for it.");
                }
                println!("Run `inkentry status` to check progress.");
                if let Some(p) = log_in_use {
                    println!("  Log: {}", p.display());
                }
            } else {
                println!(
                    "Index: {} files, {} chunks. Started a background process to embed {} \
                     chunk(s), but another `inkentry index` run claimed this project's lock \
                     before it could take over.",
                    stats.file_count, stats.chunk_count, pending,
                );
                println!(
                    "Those chunks may be left unembedded. Run `inkentry index` again once the \
                     other run finishes to pick them up."
                );
            }
            return Ok(());
        }
        // Spawn failed: falls through inline without the run lock; re-acquiring
        // would only move the same race.
    }

    if tier.is_server() && embed_ready {
        // Lets `inkentry status` in another terminal report the embed as running.
        let worker_guard = super::embed_worker::EmbedWorkerGuard::acquire(&db, &db_path);
        embed_phase::run_embed_phase(queue, &db, &cfg, &tier, &project_root, args.batch_size, &mp)
            .await?;
        drop(worker_guard);
    } else {
        phases::eprint_embed_skipped_notice(&tier, &cfg);
    }

    let stats = db.stats()?;
    println!(
        "\nIndex: {} files, {} chunks, {} embeddings",
        stats.file_count, stats.chunk_count, stats.embedding_count
    );

    if result.indexed > 100 {
        eprintln!("Spawning background job for title-less refinement and conventions\u{2026}");
        let log = continuation::background_log_path(&db_path);
        let mut cmd = continuation::build_detached_child_command(
            &std::env::current_exe()?,
            "--_background-phases",
            &args,
        );
        let in_use = continuation::redirect_to_background_log(&mut cmd, log.as_deref());
        if let Some(p) = in_use {
            eprintln!("  Log: {}", p.display());
        }
        drop(run_lock.take());
        crash_test_hook::pause_at("after_run_lock_drop", "background_phases");
        let _std_handles = super::helpers::StdHandlesNotInherited::for_spawn();
        match cmd.spawn() {
            Ok(child) => {
                if run_lock::wait_for_holder_pid(
                    &inkentry_dir,
                    child.id(),
                    continuation::HANDOFF_CONFIRM_TIMEOUT,
                    continuation::HANDOFF_POLL_INTERVAL,
                ) {
                    return Ok(());
                }
                eprintln!(
                    "Warning: another `inkentry index` run claimed this project's lock before \
                     the background job could take over; title-less refinement and convention \
                     extraction were not completed. Run `inkentry index` again once the other run \
                     finishes."
                );
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("failed to spawn background indexer; running inline: {e}");
            }
        }
    }

    phases::run_post_embed_phases(&args, &cfg, &db, &project_root, &root_canonical, &db_path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        index: IndexArgs,
    }

    #[test]
    fn batch_size_flag_is_captured() {
        let cli = TestCli::try_parse_from(["inkentry", "some/path", "--batch-size", "16"])
            .expect("parse");
        assert_eq!(cli.index.batch_size, 16);
    }

    #[test]
    fn batch_size_defaults_to_zero_meaning_calibrated_with_no_user_cap() {
        let cli = TestCli::try_parse_from(["inkentry", "some/path"]).expect("parse");
        assert_eq!(cli.index.batch_size, 0);
    }
}
