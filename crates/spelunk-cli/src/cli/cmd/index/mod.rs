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

    /// Skip LLM summary generation even when server_url is configured
    #[arg(long)]
    pub no_summaries: bool,

    /// Number of chunks to send to the LLM per summary request (default: 10)
    #[arg(long, default_value = "10")]
    pub summary_batch_size: usize,

    /// Internal: run only phases 3-5 (graph rank, summaries).
    /// Used by the background process spawned after a large foreground index.
    #[arg(long = "_background-phases", hide = true, default_value_t = false)]
    pub background_phases: bool,

    /// Internal: skip parsing and run only the embed phase (plus phases 3-5)
    /// against the chunks already stored in the index. Used by the subprocess
    /// spawned for `--detach-embed`, which rebuilds the embed queue from the DB.
    #[arg(long = "_embed-phases", hide = true, default_value_t = false)]
    pub embed_phases: bool,

    /// Detach immediately: re-exec spelunk in the background and return.
    /// Useful in git hooks so the hook does not block the git process.
    #[arg(long, default_value_t = false)]
    pub detach: bool,

    /// Parse in the foreground, then hand the (usually long) embedding phase to
    /// a detached background process and return the prompt. Confirm completion
    /// later with `spelunk status` (it reports "embedding in progress" while the
    /// detached run has chunks left to embed).
    #[arg(long, default_value_t = false)]
    pub detach_embed: bool,

    /// The `--config` override this process itself resolved, if any. Not part
    /// of the `index` subcommand's own argv: `--config` is a global `Cli`-level
    /// flag, so `main` fills this in after parsing. Threaded through so the
    /// detached-child spawns below can forward the same override rather than
    /// have the child re-resolve the default config.
    #[arg(skip)]
    pub config_path: Option<PathBuf>,
}

use crate::{capability, config::Config, registry::Registry, storage::Database};

mod crash_test_hook;
mod embed_phase;
mod mentions;
mod parse_phase;
mod run_lock;
mod summaries;
mod worktree;

pub async fn index(args: IndexArgs, cfg: Config) -> Result<()> {
    if args.detach {
        super::helpers::spawn_detached()?;
        return Ok(());
    }

    // Validate config: server_url requires project_id.
    cfg.validate()?;

    // Compile secret-scanning regexes once before the hot loop.
    crate::indexer::secrets::init();

    // If running inside a git linked worktree, resolve to the main worktree root
    // so all worktrees share one index without creating any symlink.
    let project_root = worktree::resolve_main_worktree_root(&args.path);

    // Default DB lives inside the project root, scoping the index to the project.
    let db_path = args
        .db
        .clone()
        .unwrap_or_else(|| project_root.join(".spelunk").join("index.db"));

    // Serialize whole `spelunk index` runs against this project: two
    // concurrent writers reproducibly corrupt index.db (see run_lock.rs doc
    // comment), so only one process may hold this at a time. `mut` + `Option`
    // because the two background-spawn sites below explicitly release it
    // before handing off to a continuation child (see their comments).
    let spelunk_dir = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| project_root.join(".spelunk"));
    let mut run_lock = match run_lock::try_acquire(&spelunk_dir)? {
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
                         \n  spelunk index {} --force\n",
                        db_path.display(),
                        args.path.display(),
                    )
                });
            }
        }
    };

    // Keep the global registry in sync with the current location.
    {
        let root_now = spelunk_core::utils::canonicalize(args.path.as_ref());
        let db_now = spelunk_core::utils::canonicalize(db_path.as_ref());
        if let Ok(reg) = Registry::open() {
            let _ = reg.register(&root_now, &db_now);
        }
    }

    // --recount: backfill token_count for existing chunks, then exit.
    if args.recount {
        let updated = db.backfill_token_counts()?;
        println!("Backfilled token counts for {updated} chunk(s).");
        return Ok(());
    }

    // Canonicalise the root so symlinks don't create duplicate entries.
    let root_canonical = spelunk_core::utils::canonicalize(args.path.as_ref());

    // ── Background-phases mode ────────────────────────────────────────────────
    // When spawned as a background process (--_background-phases), skip phases
    // 1 & 2 (walk, parse, embed) which are already done, and run only phases 3–5.
    if args.background_phases {
        run_background_phases(&args, &cfg, &db, &root_canonical, &db_path).await?;
        return Ok(());
    }

    // ── Embed-phases mode (detached embed) ────────────────────────────────────
    // Spawned by `--detach-embed` after the foreground process finished
    // parsing: skip phase 1 (parse) and rebuild the embed queue from the chunks
    // already stored in the DB, then run the embed phase and phases 3–5.
    if args.embed_phases {
        run_embed_phases(&args, &cfg, &db, &project_root, &root_canonical, &db_path).await?;
        return Ok(());
    }

    let mp = MultiProgress::new();

    // ── Phase 1: parse + store chunks ────────────────────────────────────────
    let result = parse_phase::run_parse_phase(&root_canonical, &db, &args, &mp, &cfg)?;
    if result.removed > 0 {
        eprintln!("Removed {} stale file(s) from index.", result.removed);
    }

    // ── Phase 2: embed chunks (Tier 1 only) ─────────────────────────────────
    //
    // `get_inference_tier` (not `get_tier`): local_first always prefers the
    // local loopback embedder for inference, even with an explicit
    // server_url set (2026-07-23 founder decision). `get_tier` alone would
    // probe the explicit server_url and hand its (possibly wrong) tier
    // straight to the batch-calibrated embed request loop below.
    let tier = capability::get_inference_tier(&cfg).await;

    if result.chunk_ids_and_texts.is_empty() {
        let stats = db.stats()?;
        println!(
            "Index: {} files, {} chunks, {} embeddings (nothing new to process)",
            stats.file_count, stats.chunk_count, stats.embedding_count
        );
        return Ok(());
    }

    // Embed only when the server's embedder is actually ready to serve
    // (`caps.index_embed` is advertised only in the `ready` state). When the
    // server is reachable but the model is still `loading` or has failed
    // (`unavailable`), skip embedding and print a visible, differentiated
    // notice rather than letting the embed request 503 out mid-index or
    // silently producing an unembedded index.
    let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);

    // ── Detached embed ────────────────────────────────────────────────────────
    // Parsing is done and the chunks are persisted; hand the (usually long)
    // embedding phase to a background process so the user regains the prompt
    // now. The subprocess (`--_embed-phases`) rebuilds the embed queue from the
    // DB, so nothing from `result` needs to cross the process boundary. Confirm
    // completion later with `spelunk status`.
    //
    // The spawn is gated on "worth waiting for" (ready OR still loading), not
    // on ready alone: the worker owns the readiness wait, and a fresh install
    // arrives here with the embedder still `loading`. Gating the spawn on
    // `embed_ready` is exactly the no-op that ships a permanently unembedded
    // index on a cold machine.
    if args.detach_embed && tier.is_server() && detach_embed_eligible(&tier) {
        let embed_log = background_log_path(&db_path);
        // Release before spawning: the child re-acquires this same lock on
        // entry, and dropping first (rather than after `spawn()` returns) is
        // what makes that a race-free handoff instead of a timing-dependent
        // one - the child starts running concurrently with us the instant
        // `spawn()` is called, so releasing any later can lose the race.
        drop(run_lock.take());
        if let EmbedSpawn::Detached(log_in_use) =
            spawn_embed_subprocess(&args, embed_log.as_deref())?
        {
            let stats = db.stats()?;
            let pending = stats.chunk_count - stats.embedding_count;
            println!(
                "Index: {} files, {} chunks. Embedding {} chunk(s) in the background\u{2026}",
                stats.file_count, stats.chunk_count, pending,
            );
            if !embed_ready {
                println!("The embedder is still loading; the background worker waits for it.");
            }
            println!("Run `spelunk status` to check progress.");
            if let Some(p) = log_in_use {
                println!("  Log: {}", p.display());
            }
            return Ok(());
        }
        // Spawn failed: fall through to the inline path (embeds now if ready,
        // else prints the skip notice), unprotected by the run lock already
        // dropped above. Accepted: `Command::spawn` only fails on resource
        // exhaustion, and re-acquiring here would just move the same
        // race-vs-a-real-child problem rather than remove it.
    }

    if tier.is_server() && embed_ready {
        // Liveness marker so `spelunk status` from another terminal reports a
        // foreground embed as running rather than telling the user to resume.
        let worker_guard = super::embed_worker::EmbedWorkerGuard::acquire(&db, &db_path);
        embed_phase::run_embed_phase(
            result.chunk_ids_and_texts,
            &db,
            &cfg,
            &tier,
            &project_root,
            args.batch_size,
            &mp,
        )
        .await?;
        drop(worker_guard);
    } else {
        eprint_embed_skipped_notice(&tier, &cfg);
    }

    let stats = db.stats()?;
    println!(
        "\nIndex: {} files, {} chunks, {} embeddings",
        stats.file_count, stats.chunk_count, stats.embedding_count
    );

    // ── Background spawn for phases 3–5 ──────────────────────────────────────
    // When more than 100 files were newly indexed, detach phases 3-5 into a
    // background process so the user regains the prompt immediately.
    if result.indexed > 100 {
        eprintln!("Spawning background job for graph rank, spec discovery, and summaries\u{2026}");
        let log = background_log_path(&db_path);
        let mut cmd =
            build_detached_child_command(&std::env::current_exe()?, "--_background-phases", &args);
        let in_use = redirect_to_background_log(&mut cmd, log.as_deref());
        if let Some(p) = in_use {
            eprintln!("  Log: {}", p.display());
        }
        // Release before spawning, same reasoning as the detach-embed site
        // above: the child re-acquires this lock on entry, and only
        // dropping first makes that race-free.
        drop(run_lock.take());
        if cmd.spawn().is_ok() {
            return Ok(());
        }
        // Fall through and run phases 3-5 inline as fallback, unprotected by
        // the run lock already dropped above (see the detach-embed site's
        // comment on this same tradeoff).
        tracing::warn!("failed to spawn background indexer; running inline");
    }

    run_phases_3_to_5(&args, &cfg, &db, &root_canonical, &db_path).await
}

/// Log for the detached phases-3–5 child, beside the index it reports on.
fn background_log_path(db_path: &std::path::Path) -> Option<std::path::PathBuf> {
    db_path.parent().map(|d| d.join("index-background.log"))
}

/// Point the detached child's stdout+stderr at `log`, returning the path
/// actually in use. Falls back to a null sink when the log cannot be opened,
/// since diagnostics are best-effort and must never fail the index.
///
/// Inheriting the parent's streams instead is not an option: a pipe reader
/// (`git commit`, CI) blocks until the detached child exits, and a reader that
/// closes first SIGPIPEs the child mid-phase.
fn redirect_to_background_log<'a>(
    cmd: &mut std::process::Command,
    log: Option<&'a std::path::Path>,
) -> Option<&'a std::path::Path> {
    // stdout and stderr need independent handles onto the same file.
    let opened = log.and_then(|p| {
        let out = super::helpers::open_private_file_for_write(p).ok()?;
        let err = out.try_clone().ok()?;
        Some((p, out, err))
    });
    match opened {
        Some((path, out, err)) => {
            cmd.stdout(out).stderr(err);
            Some(path)
        }
        None => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            None
        }
    }
}

/// Outcome of the detached embed spawn.
enum EmbedSpawn<'a> {
    /// Spawn failed; caller embeds inline.
    Inline,
    /// Running detached, with the diagnostics log actually in use (if any) so
    /// the caller can point the user at it.
    Detached(Option<&'a std::path::Path>),
}

/// Build the argv shared by every detached re-exec that continues indexing in
/// a child process: the child parses its own fresh `IndexArgs`/`Config` from
/// this argv rather than inheriting the parent's already-parsed values, so
/// anything the parent resolved that isn't a plain pass-through of `args`
/// itself (the global `--config` override) or on this list (`--no-summaries`,
/// `--summary-batch-size`) would otherwise silently reset to its default in
/// the child. `mode_flag` selects which internal phase-only mode the child
/// runs (`--_background-phases` or `--_embed-phases`); callers append any
/// mode-specific flags (e.g. `--batch-size` for the embed phase) afterwards.
///
/// Env vars and cwd are not part of this contract: `std::process::Command`
/// inherits both by default and nothing here calls `.env_clear()` or
/// `.current_dir()` to opt out (see the regression test below).
fn build_detached_child_command(
    exe: &std::path::Path,
    mode_flag: &str,
    args: &IndexArgs,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("index");
    cmd.arg(&args.path);
    cmd.arg(mode_flag);
    if let Some(db_arg) = &args.db {
        cmd.args(["--db", &db_arg.to_string_lossy()]);
    }
    if let Some(cfg_path) = &args.config_path {
        cmd.args(["--config", &cfg_path.to_string_lossy()]);
    }
    if args.no_summaries {
        cmd.arg("--no-summaries");
    }
    cmd.args(["--summary-batch-size", &args.summary_batch_size.to_string()]);
    cmd.stdin(std::process::Stdio::null());
    cmd
}

/// Spawn a detached background process to run the embed phase (plus phases 3–5)
/// against the chunks the foreground run just parsed, reusing the internal
/// `--_embed-phases` mode. Mirrors the phases-3–5 background spawn: the parent
/// regains its prompt immediately and the child's diagnostics go to `log`.
fn spawn_embed_subprocess<'a>(
    args: &IndexArgs,
    log: Option<&'a std::path::Path>,
) -> Result<EmbedSpawn<'a>> {
    let mut cmd = build_detached_child_command(&std::env::current_exe()?, "--_embed-phases", args);
    cmd.args(["--batch-size", &args.batch_size.to_string()]);
    let in_use = redirect_to_background_log(&mut cmd, log);
    match cmd.spawn() {
        Ok(_) => Ok(EmbedSpawn::Detached(in_use)),
        Err(e) => {
            tracing::warn!("failed to spawn detached embed process; embedding inline: {e}");
            Ok(EmbedSpawn::Inline)
        }
    }
}

/// True when handing the embed pass to the detached worker can do useful work:
/// the embedder is `ready`, or still `loading` (the worker owns the readiness
/// wait, see [`wait_for_embedder`]). `unavailable` and `disabled` are terminal
/// for this server process, and an older server that never advertises
/// `index.embed` has nothing to wait for.
fn detach_embed_eligible(tier: &capability::Tier) -> bool {
    matches!(tier.caps(), Some(c) if c.index_embed)
        || matches!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Loading)
        )
}

/// First delay of the embed worker's readiness-wait backoff.
const EMBED_WAIT_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
/// Backoff growth is bounded at this interval; the wait itself is not
/// time-bounded while the embedder reports `loading` (a model download can
/// legitimately take many minutes, and the queue is durable).
const EMBED_WAIT_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
/// Consecutive offline probes tolerated before the worker concludes the server
/// is gone (crashed after spawning us) rather than momentarily unreachable.
const EMBED_WAIT_MAX_OFFLINE_PROBES: u32 = 10;

/// Wait until the server's embedder can serve, polling `/v1/health` with a
/// bounded backoff. Returns the final observed tier; the caller re-derives
/// `index_embed` from it.
///
/// A not-ready embedder is a transient condition to wait on, not a terminal
/// condition to skip: `ensure_server_running` waits for liveness only (health
/// goes live at socket bind, before the model loads), so a fresh machine
/// reaches the worker with the embedder still `loading`. Only `unavailable`
/// and `disabled` (or a server with no embedder at all) are terminal; each
/// keeps its distinct notice via `eprint_embed_skipped_notice`. `loading` is
/// never a reason to abandon durable queued work.
///
/// `get_inference_tier_fresh` (not `probe_tier_fresh`): local_first always
/// prefers the local loopback embedder, even with an explicit server_url set
/// (2026-07-23 founder decision), and this poller must keep re-observing
/// that same local-vs-remote routing decision on every iteration rather than
/// freezing on `get_tier`'s cached first probe of an unrelated server_url.
async fn wait_for_embedder(
    cfg: &Config,
    initial_backoff: std::time::Duration,
    max_backoff: std::time::Duration,
) -> capability::Tier {
    let mut backoff = initial_backoff;
    let mut offline_probes = 0u32;
    let mut announced = false;
    loop {
        let tier = capability::get_inference_tier_fresh(cfg).await;
        match &tier {
            capability::Tier::Server { .. } => {
                if matches!(tier.caps(), Some(c) if c.index_embed) {
                    return tier;
                }
                if !matches!(
                    tier.embedder_state(),
                    Some(capability::EmbedderState::Loading)
                ) {
                    // unavailable / disabled / no embedder: terminal here.
                    return tier;
                }
                offline_probes = 0;
                if !announced {
                    eprintln!("Waiting for the embedder to finish loading\u{2026}");
                    announced = true;
                }
            }
            capability::Tier::Offline => {
                offline_probes += 1;
                if offline_probes >= EMBED_WAIT_MAX_OFFLINE_PROBES {
                    return tier;
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Embed-only entry point for the detached `--_embed-phases` subprocess: rebuild
/// the embed queue from the chunks already in the DB (no re-parse), wait for
/// the embedder to become ready, run the embed phase, then phases 3–5.
async fn run_embed_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Liveness marker for `spelunk status` (dropped on exit; a killed worker
    // leaves it behind for status to classify as a dead pid). Held through the
    // readiness wait too: a worker waiting on a loading embedder is running,
    // and status must not advise a resume that would double it up.
    let worker_guard = super::embed_worker::EmbedWorkerGuard::acquire(db, db_path);

    let tier = wait_for_embedder(cfg, EMBED_WAIT_INITIAL_BACKOFF, EMBED_WAIT_MAX_BACKOFF).await;
    let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);
    if tier.is_server() && embed_ready {
        let chunk_ids_and_texts = parse_phase::missing_embedding_texts(db)?;
        if !chunk_ids_and_texts.is_empty() {
            let mp = MultiProgress::new();
            embed_phase::run_embed_phase(
                chunk_ids_and_texts,
                db,
                cfg,
                &tier,
                project_root,
                args.batch_size,
                &mp,
            )
            .await?;
        }
    } else {
        eprint_embed_skipped_notice(&tier, cfg);
    }
    drop(worker_guard);

    run_phases_3_to_5(args, cfg, db, root_canonical, db_path).await
}

/// Build the differentiated notice lines shown when the embedding phase is
/// skipped, so an unembedded index is never a silent surprise. Pure so it can
/// be unit-tested; the four cases mirror the server's readiness contract.
/// `server_url` is `cfg.server_url` (used only for the offline case).
///
/// `remote_url` is `Some` when the probed server came from an explicit
/// `server_url` (not loopback auto-discovery). The unavailable-embedder
/// notice must then name that server instead of pointing at `spelunk server
/// logs`, which only reads the local auto-daemon's log and would show clean
/// logs for a failure that lives on the remote server.
///
/// `is_windows` gates the Windows Defender Firewall hint in the offline
/// case: that hint is a real cause only on Windows, and printing it on every
/// platform actively misdirects a macOS/Linux user away from the real
/// problem (an unreachable configured `server_url`). Callers pass
/// `cfg!(windows)`; injected here so the platform-specific behaviour is
/// testable without `#[cfg(windows)]` test gating.
fn embed_skipped_lines(
    embedder_state: Option<capability::EmbedderState>,
    server_url: Option<&str>,
    remote_url: Option<&str>,
    is_windows: bool,
) -> Vec<String> {
    use capability::EmbedderState;
    match embedder_state {
        Some(EmbedderState::Loading) => vec![
            "Note: the embedder is still warming up — chunks indexed for text/ast-grep search."
                .to_string(),
            "Re-run `spelunk index` in a moment to add embeddings (check `spelunk server status`)."
                .to_string(),
        ],
        Some(EmbedderState::Unavailable) => match remote_url {
            Some(url) => vec![
                format!(
                    "Warning: the embedder failed to load on team server {url}; chunks indexed \
                     for text/ast-grep search only."
                ),
                "Check that server's own logs for the load error, then re-run `spelunk index`."
                    .to_string(),
            ],
            None => vec![
                "Warning: the embedder failed to load; chunks indexed for text/ast-grep search \
                 only."
                    .to_string(),
                "See `spelunk server logs` for the load error, then re-run `spelunk index`."
                    .to_string(),
            ],
        },
        // Reachable server without a ready embedder for any other reason
        // (`disabled`, or an older server that never advertised `index.embed`).
        Some(_) => vec![
            "Note: this server has no embedder — chunks indexed for text/ast-grep search only."
                .to_string(),
        ],
        // Offline: no server reachable. Reaching this arm with `server_url`
        // set means the probe took the explicit-URL path (see
        // `capability::probe::probe`): an auto-discovered loopback miss never
        // carries a `server_url`, so the message can unconditionally say
        // "configured server_url" rather than guessing.
        None => {
            if let Some(url) = server_url {
                let mut lines = vec![format!(
                    "Warning: server_url is explicitly configured to {url}, which is \
                     unreachable, so the embedding phase is skipped. This overrides the \
                     auto-discovered local server, so a healthy `spelunk server start` \
                     daemon elsewhere will not be used while server_url is set."
                )];
                if is_windows {
                    lines.push(
                        "On Windows, allow the loopback listener through Defender Firewall \
                         (accept the prompt on `spelunk server start`)."
                            .to_string(),
                    );
                }
                lines.push(
                    "Chunks are indexed for text/ast-grep search. Re-run `spelunk index` once \
                     the server is reachable to add embeddings."
                        .to_string(),
                );
                lines
            } else {
                vec![
                    "Note: start a local server (`spelunk server start`) to enable semantic search."
                        .to_string(),
                ]
            }
        }
    }
}

/// Print the embed-skipped notice to stderr.
fn eprint_embed_skipped_notice(tier: &capability::Tier, cfg: &Config) {
    for line in embed_skipped_lines(
        tier.embedder_state(),
        cfg.server_url.as_deref(),
        tier.explicit_remote_url(),
        cfg!(windows),
    ) {
        eprintln!("{line}");
    }
}

// ── Phases 3–5 (shared between inline and background-phases mode) ─────────────

async fn run_phases_3_to_5(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    // Phase 3: PageRank
    eprintln!("Computing graph rank…");
    let edges = db.graph_edges_all()?;
    if !edges.is_empty() {
        let pr_scores = crate::indexer::pagerank::compute_pagerank(&edges, 20, 0.85);
        let named_chunks = db.chunks_with_names()?;
        let updates: Vec<(i64, f32)> = named_chunks
            .into_iter()
            .filter_map(|(id, name)| name.and_then(|n| pr_scores.get(&n).copied().map(|s| (id, s))))
            .collect();
        if !updates.is_empty() {
            db.update_graph_ranks(&updates)?;
        }
    }

    // Phase 4: LLM summaries. Must finish before the process exits: an in-flight
    // summary is silently lost. Backgrounding here is process-level
    // (--detach, --detach-embed, the phases-3-5 spawn), never a thread.
    if let Err(e) =
        summaries::generate_summaries(args.no_summaries, args.summary_batch_size, cfg, db).await
    {
        eprintln!("Warning: summary generation failed: {e:#}");
    }

    // Phase 5: convention extraction (heuristic, no LLM).
    eprintln!("Extracting conventions\u{2026}");
    match crate::conventions::run_extraction(db) {
        Ok(records) => {
            if !records.is_empty() {
                eprintln!("Conventions: {} record(s) detected.", records.len());
            }
        }
        Err(e) => tracing::warn!("convention extraction failed (non-fatal): {e}"),
    }

    // Register / update this project in the global registry.
    if let Ok(reg) = Registry::open() {
        let db_canonical = spelunk_core::utils::canonicalize(db_path);
        if let Err(e) = reg.register(root_canonical, &db_canonical) {
            tracing::warn!("registry update failed: {e}");
        }
    }
    Ok(())
}

async fn run_background_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    run_phases_3_to_5(args, cfg, db, root_canonical, db_path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Minimal parser wrapper so we can exercise `IndexArgs` clap parsing in
    /// isolation without pulling in the whole top-level `Cli`.
    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(flatten)]
        index: IndexArgs,
    }

    #[test]
    fn batch_size_flag_is_captured() {
        // The user-supplied `--batch-size` must land in `IndexArgs.batch_size`,
        // which `index()` then threads into `run_embed_phase`. Before this fix
        // the value was parsed but never passed through (silent no-op).
        let cli =
            TestCli::try_parse_from(["spelunk", "some/path", "--batch-size", "16"]).expect("parse");
        assert_eq!(cli.index.batch_size, 16);
    }

    #[test]
    fn batch_size_defaults_to_zero_meaning_calibrated_with_no_user_cap() {
        // 0 means "no user-supplied cap" — the embed phase calibrates the
        // batch size from measured throughput up to the server's own 256-chunk
        // ceiling, rather than being pinned to a fixed default (see
        // `resolve_batch_ceiling` in embed_phase.rs).
        let cli = TestCli::try_parse_from(["spelunk", "some/path"]).expect("parse");
        assert_eq!(cli.index.batch_size, 0);
    }

    // ── build_detached_child_command: shared re-exec contract ───────────────────

    fn sample_index_args() -> IndexArgs {
        TestCli::try_parse_from(["spelunk", "some/path"])
            .expect("parse")
            .index
    }

    #[test]
    fn detached_child_command_inherits_cwd_and_env() {
        // `std::process::Command` inherits both by default; this only breaks
        // if a future edit adds `.current_dir(...)` or `.env_clear()`/`.env(...)`
        // to the shared builder.
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/spelunk"),
            "--_background-phases",
            &sample_index_args(),
        );
        assert!(
            cmd.get_current_dir().is_none(),
            "must inherit the parent's cwd rather than pin one"
        );
        assert!(
            cmd.get_envs().next().is_none(),
            "must inherit the parent's environment rather than clear or override it"
        );
    }

    #[test]
    fn detached_child_command_forwards_config_path_when_resolved() {
        // Before the fix, `IndexArgs` had no config-path field at all, so
        // neither spawn could forward a resolved `--config` override and the
        // child re-resolved the default config instead.
        let mut args = sample_index_args();
        args.config_path = Some(std::path::PathBuf::from("/tmp/custom-config.toml"));
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/spelunk"),
            "--_background-phases",
            &args,
        );
        let argv: Vec<_> = cmd.get_args().collect();
        let pos = argv
            .iter()
            .position(|a| *a == "--config")
            .expect("--config must be forwarded when the parent resolved an override");
        assert_eq!(argv[pos + 1], "/tmp/custom-config.toml");
    }

    #[test]
    fn detached_child_command_omits_config_flag_when_not_resolved() {
        // A default-config run must not force an explicit `--config` onto the
        // child: `config_path` is `None` when the user passed no override, and
        // an unconditional `--config` would stop the child from resolving its
        // own default the way the parent did.
        let args = sample_index_args();
        assert!(args.config_path.is_none());
        let cmd = build_detached_child_command(
            std::path::Path::new("/usr/bin/spelunk"),
            "--_background-phases",
            &args,
        );
        let argv: Vec<_> = cmd.get_args().collect();
        assert!(
            !argv.iter().any(|a| *a == "--config"),
            "must not add --config when the parent had no override"
        );
    }

    #[test]
    fn detached_child_command_forwards_no_summaries_to_both_spawn_sites() {
        // Before the fix the phases-3-5 background spawn built its argv
        // independently and never included `--no-summaries` at all (only the
        // embed-phase spawn did), so disabling summaries still let the
        // background child generate them.
        let mut args = sample_index_args();
        args.no_summaries = true;
        for mode_flag in ["--_background-phases", "--_embed-phases"] {
            let cmd = build_detached_child_command(
                std::path::Path::new("/usr/bin/spelunk"),
                mode_flag,
                &args,
            );
            let argv: Vec<_> = cmd.get_args().collect();
            assert!(
                argv.iter().any(|a| *a == "--no-summaries"),
                "--no-summaries must reach the {mode_flag} child"
            );
        }
    }

    #[test]
    fn detached_child_command_forwards_configured_summary_batch_size_to_both_spawn_sites() {
        // Before the fix neither spawn forwarded `--summary-batch-size`, so a
        // custom value silently reset to the default (10) in whichever child
        // ran phase 4.
        let args = TestCli::try_parse_from(["spelunk", "some/path", "--summary-batch-size", "42"])
            .expect("parse")
            .index;
        assert_eq!(args.summary_batch_size, 42);
        for mode_flag in ["--_background-phases", "--_embed-phases"] {
            let cmd = build_detached_child_command(
                std::path::Path::new("/usr/bin/spelunk"),
                mode_flag,
                &args,
            );
            let argv: Vec<_> = cmd.get_args().collect();
            let pos = argv
                .iter()
                .position(|a| *a == "--summary-batch-size")
                .expect("--summary-batch-size must be forwarded");
            assert_eq!(argv[pos + 1], "42");
        }
    }

    // ── embed_skipped_lines: 0-chunks / offline notice (#5) ─────────────────────

    #[test]
    fn embed_skipped_loading_advises_retry() {
        let lines =
            embed_skipped_lines(Some(capability::EmbedderState::Loading), None, None, false);
        assert!(!lines.is_empty(), "notice must not be silent");
        let joined = lines.join("\n");
        assert!(joined.contains("warming up"));
        assert!(joined.contains("Re-run `spelunk index`"));
    }

    #[test]
    fn embed_skipped_unavailable_loopback_points_at_logs() {
        // Loopback auto-discovery: the failing embedder IS the local daemon,
        // so `spelunk server logs` is the right place to look.
        let lines = embed_skipped_lines(
            Some(capability::EmbedderState::Unavailable),
            None,
            None,
            false,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("failed to load"));
        assert!(joined.contains("spelunk server logs"));
    }

    #[test]
    fn embed_skipped_unavailable_remote_names_that_server_never_local_logs() {
        // Explicit server_url: `spelunk server logs` reads the LOCAL daemon's
        // log, which is clean when the failure lives on the team server. The
        // notice must name the probed server instead.
        let lines = embed_skipped_lines(
            Some(capability::EmbedderState::Unavailable),
            None,
            Some("https://team.example:7777"),
            false,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("failed to load"));
        assert!(
            joined.contains("https://team.example:7777"),
            "got: {joined}"
        );
        assert!(
            !joined.contains("spelunk server logs"),
            "must not point a remote failure at local logs: {joined}"
        );
    }

    #[test]
    fn embed_skipped_unreachable_server_names_configured_server_url() {
        // Offline (no reachable server) with a configured server_url: the notice
        // must name the actual URL attempted AND say explicitly that it came
        // from a configured `server_url` (not the auto-discovered loopback
        // daemon). Without this, a user with a healthy loopback daemon running
        // has no path from the message to the real cause: the daemon was
        // never being used because server_url overrides it.
        let lines = embed_skipped_lines(None, Some("http://127.0.0.1:7777"), None, false);
        let joined = lines.join("\n");
        assert!(joined.contains("http://127.0.0.1:7777"), "got: {joined}");
        assert!(joined.contains("unreachable"), "got: {joined}");
        assert!(joined.contains("server_url"), "got: {joined}");
        assert!(
            joined.contains("configured"),
            "must say the target came from a *configured* server_url, not just name \
             `server_url` in passing (this is the specific wording the defect asked for, \
             distinguishing it from the auto-discovered daemon): got: {joined}"
        );
        assert!(
            joined.contains("overrides") || joined.contains("override"),
            "must explain that an explicit server_url overrides the auto-discovered \
             local daemon, so a healthy daemon elsewhere is not the fix: got: {joined}"
        );
    }

    #[test]
    fn embed_skipped_unreachable_server_shows_firewall_hint_only_on_windows() {
        // The Windows Defender Firewall hint is a real cause ONLY on Windows;
        // printing it unconditionally (the field bug, hit on macOS) actively
        // misdirects a user on any other platform.
        let windows_lines = embed_skipped_lines(None, Some("http://127.0.0.1:7777"), None, true);
        assert!(
            windows_lines.join("\n").contains("Firewall"),
            "the Windows hint must still show when the host platform is Windows"
        );

        let non_windows_lines =
            embed_skipped_lines(None, Some("http://127.0.0.1:7777"), None, false);
        assert!(
            !non_windows_lines.join("\n").contains("Firewall"),
            "the Windows-only hint must not print on a non-Windows host: got: {:?}",
            non_windows_lines
        );
    }

    #[test]
    fn embed_skipped_no_server_suggests_starting_one() {
        let lines = embed_skipped_lines(None, None, None, false);
        let joined = lines.join("\n");
        assert!(joined.contains("spelunk server start"));
    }

    // ── detach_embed_eligible: the spawn gate must include `loading` ────────────

    fn tier_with(embed_ready: bool, state: capability::EmbedderState) -> capability::Tier {
        let mut caps = capability::Capabilities::all();
        caps.index_embed = embed_ready;
        capability::Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps,
            auto_discovered: true,
            embedder_state: state,
            server_limits: None,
        }
    }

    #[test]
    fn detach_eligible_when_embedder_ready() {
        assert!(detach_embed_eligible(&tier_with(
            true,
            capability::EmbedderState::Ready
        )));
    }

    #[test]
    fn detach_eligible_when_embedder_still_loading() {
        // The cold-start case ADR-070 D1/D2 exists for: a server started
        // moments ago advertises no index.embed yet, but the worker can wait
        // it out. Gating the spawn on readiness alone is the recorded no-op.
        assert!(detach_embed_eligible(&tier_with(
            false,
            capability::EmbedderState::Loading
        )));
    }

    #[test]
    fn detach_not_eligible_for_terminal_embedder_states() {
        for state in [
            capability::EmbedderState::Unavailable,
            capability::EmbedderState::Disabled,
            capability::EmbedderState::Unknown,
        ] {
            assert!(
                !detach_embed_eligible(&tier_with(false, state)),
                "state {state:?} is terminal; spawning a worker would wait forever"
            );
        }
    }

    #[test]
    fn detach_not_eligible_offline() {
        assert!(!detach_embed_eligible(&capability::Tier::Offline));
    }

    // ── wait_for_embedder: the worker owns the readiness wait (ADR-070 D2) ────

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `/v1/health` body for an embedder in `state`. `index.embed` is
    /// advertised only when ready, mirroring the real server's contract.
    fn health_body(state: &str) -> serde_json::Value {
        let (caps, dim) = if state == "ready" {
            (
                vec!["memory", "index.embed", "search.semantic"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )
        } else {
            (vec!["memory"], 0)
        };
        serde_json::json!({
            "status": "ok",
            "version": "0.9.3",
            "capabilities": caps,
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "embedding_dim": dim,
            "embedder": { "state": state, "detail": null }
        })
    }

    // `mode = "cloud_first"`: every test below drives the wait loop's polling
    // logic (loading/ready/unavailable/disabled transitions, the offline
    // give-up bound) by mocking `/v1/health` directly at `url` and expecting
    // `wait_for_embedder` to probe exactly that URL. Under the default
    // `local_first` mode, `get_inference_tier_fresh` routes inference to the
    // local loopback embedder instead and never touches `server_url` at all
    // (see `wait_for_embedder_local_first_routes_loopback_transition_not_server_url`
    // below for that path); `cloud_first` is the mode where an explicit
    // `server_url` legitimately serves inference, which is what every test
    // here needs to still be exercising the polling logic against `url`.
    fn cfg_for(url: String) -> Config {
        Config {
            server_url: Some(url),
            project_id: Some("local/test".to_string()),
            mode: Some(crate::config::SyncMode::CloudFirst),
            ..Default::default()
        }
    }

    const TEST_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1);

    #[tokio::test]
    async fn wait_for_embedder_outlasts_a_loading_embedder() {
        // The readiness gate the cold-start bug lives behind: health reports
        // `loading` (twice here) before flipping to `ready`. The wait must
        // keep polling through `loading` and come back with `index.embed`.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "the wait must return only once the embedder serves; got {tier:?}"
        );
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Ready)
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_treats_unavailable_as_terminal() {
        // A failed model load is terminal for this server process: return at
        // the first probe (no retries burned) and preserve the state so the
        // caller prints the distinct `unavailable` notice.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("unavailable")))
            .expect(1)
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Unavailable)
        );
        assert!(!matches!(tier.caps(), Some(c) if c.index_embed));
    }

    #[tokio::test]
    async fn wait_for_embedder_treats_disabled_as_terminal() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("disabled")))
            .expect(1)
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Disabled)
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_loading_then_unavailable_is_terminal() {
        // The embedder can flip loading -> unavailable mid-wait (model load
        // fails after the worker started polling). The wait must exit at the
        // transition with the terminal state preserved, so the caller prints
        // the distinct `unavailable` notice; it must not keep polling.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("unavailable")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert_eq!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Unavailable),
            "the terminal state observed mid-wait must be returned as-is"
        );
        assert!(!matches!(tier.caps(), Some(c) if c.index_embed));
    }

    #[tokio::test]
    async fn wait_for_embedder_offline_counter_resets_on_a_reachable_probe() {
        // The give-up counter is CONSECUTIVE offline probes, not cumulative: a
        // server that flaps (down, briefly back while loading, down again)
        // must not have its earlier misses counted against the later ones.
        // 7 offline + 1 loading + 7 offline = 14 cumulative misses, but never
        // 10 in a row, so the wait must survive to the final `ready`.
        // (A non-2xx health response probes as Tier::Offline.)
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(7)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(1)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(7)
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&mock)
            .await;

        let tier = wait_for_embedder(&cfg_for(mock.uri()), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "14 cumulative but never {EMBED_WAIT_MAX_OFFLINE_PROBES} consecutive offline \
             probes must not trip the give-up; got {tier:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_embedder_gives_up_after_bounded_offline_probes() {
        // A vanished server (crashed after spawning the worker) must not hang
        // the worker forever: bounded consecutive offline probes, then return
        // Offline so the skip notice prints and the durable queue stays put.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let dead_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        drop(listener); // port is real but nothing serves it

        let started = std::time::Instant::now();
        let tier = wait_for_embedder(&cfg_for(dead_url), TEST_BACKOFF, TEST_BACKOFF).await;
        assert!(matches!(tier, capability::Tier::Offline));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the offline give-up must be bounded"
        );
    }

    // ── wait_for_embedder backoff/give-up constants ──────────────────────────
    //
    // Every wait_for_embedder test above drives the function with
    // `TEST_BACKOFF` (1ms) so the suite doesn't take the ~150s the real
    // constants would need to reach the give-up bound. That substitution is
    // only faithful to production if the constants it stands in for keep
    // their documented values; pin them here so a silent edit (e.g. raising
    // `EMBED_WAIT_MAX_OFFLINE_PROBES` past what the give-up test's runtime
    // budget assumes) fails loudly instead of just changing real-world
    // worker wait time unnoticed. Mirrors the `loopback_probe_timeout_is_250ms`
    // -style constant pins in `capability/probe.rs`.
    #[test]
    fn embed_wait_initial_backoff_is_1s() {
        assert_eq!(EMBED_WAIT_INITIAL_BACKOFF.as_secs(), 1);
    }

    #[test]
    fn embed_wait_max_backoff_is_30s() {
        assert_eq!(EMBED_WAIT_MAX_BACKOFF.as_secs(), 30);
    }

    #[test]
    fn embed_wait_max_offline_probes_is_10() {
        assert_eq!(EMBED_WAIT_MAX_OFFLINE_PROBES, 10);
    }

    // ── wait_for_embedder: local_first routes to loopback, not server_url ────
    //
    // The routing-bug regression this story fixes: before, the wait loop
    // probed `cfg.server_url` directly (`probe_tier_fresh`) regardless of
    // mode, so a `local_first` project with an explicit `server_url` never
    // reached its local embedder from the detached worker either.

    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env, server_state_dir_env)]
    async fn wait_for_embedder_local_first_routes_loopback_transition_not_server_url() {
        // Under `local_first` (the default once `server_url` is set, with no
        // explicit `mode`), the wait loop must poll the LOCAL loopback
        // embedder, never the configured `server_url` — even while observing
        // a loading -> ready transition across several polls. `server_url` is
        // deliberately unroutable, so an accidental fallback to it surfaces
        // as a connection/DNS error, not a silent wrong-but-passing result.
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };

        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("loading")))
            .up_to_n_times(2)
            .mount(&loopback)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body("ready")))
            .mount(&loopback)
            .await;

        let loopback_port: u16 = loopback
            .uri()
            .rsplit(':')
            .next()
            .expect("uri has a port")
            .trim_end_matches('/')
            .parse()
            .expect("uri port is numeric");

        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("server.port"), format!("{loopback_port}\n")).unwrap();

        let prev_state_dir = std::env::var_os("SPELUNK_STATE_DIR");
        // SAFETY: serialised via #[serial(server_state_dir_env)] against
        // every other test touching this var.
        unsafe { std::env::set_var("SPELUNK_STATE_DIR", &state_dir) };

        let cfg = Config {
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("local/test".to_string()),
            mode: None, // defaults to local_first because server_url is set
            ..Default::default()
        };
        assert_eq!(cfg.resolve_mode(), crate::config::SyncMode::LocalFirst);

        let tier = wait_for_embedder(&cfg, TEST_BACKOFF, TEST_BACKOFF).await;

        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                None => std::env::remove_var("SPELUNK_STATE_DIR"),
            }
        }

        assert!(
            matches!(tier.caps(), Some(c) if c.index_embed),
            "the wait must observe the loopback's loading -> ready transition; got {tier:?}"
        );
        assert_eq!(
            tier.server_url(),
            Some(format!("http://127.0.0.1:{loopback_port}")).as_deref(),
            "local_first must route the wait loop to the loopback server, not the \
             configured (and unreachable) server_url; got {tier:?}"
        );
    }

    #[test]
    fn embed_skipped_is_never_silent() {
        for state in [
            Some(capability::EmbedderState::Loading),
            Some(capability::EmbedderState::Unavailable),
            Some(capability::EmbedderState::Disabled),
            Some(capability::EmbedderState::Unknown),
            None,
        ] {
            for url in [Some("http://x:1"), None] {
                for remote_url in [None, Some("https://team.example:7777")] {
                    for is_windows in [false, true] {
                        assert!(
                            !embed_skipped_lines(state, url, remote_url, is_windows).is_empty(),
                            "state {state:?} url {url:?} remote_url {remote_url:?} \
                             is_windows {is_windows} produced no notice"
                        );
                    }
                }
            }
        }
    }
}
