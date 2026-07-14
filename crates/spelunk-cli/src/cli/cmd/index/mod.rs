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

    /// Skip LLM summary generation even when llm_model is configured
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
}

use crate::{capability, config::Config, registry::Registry, storage::Database};

mod embed_phase;
mod mentions;
mod parse_phase;
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
    let result = parse_phase::run_parse_phase(&root_canonical, &db, &args, &mp)?;
    if result.removed > 0 {
        eprintln!("Removed {} stale file(s) from index.", result.removed);
    }

    // ── Phase 2: embed chunks (Tier 1 only) ─────────────────────────────────
    let tier = capability::get_tier(&cfg).await;

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
    // silently producing an unembedded index (spelunk-oss^50 #5).
    let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);
    if tier.is_server() && embed_ready {
        // ── Detached embed (spelunk-oss^74) ──────────────────────────────────
        // Parsing is done and the chunks are persisted; hand the (usually long)
        // embedding phase to a background process so the user regains the
        // prompt now. The subprocess (`--_embed-phases`) rebuilds the embed
        // queue from the DB, so nothing from `result` needs to cross the
        // process boundary. Confirm completion later with `spelunk status`.
        if args.detach_embed && spawn_embed_subprocess(&args)? {
            let stats = db.stats()?;
            let pending = stats.chunk_count - stats.embedding_count;
            println!(
                "Index: {} files, {} chunks. Embedding {} chunk(s) in the background\u{2026}",
                stats.file_count, stats.chunk_count, pending,
            );
            println!("Run `spelunk status` to check progress.");
            return Ok(());
        }

        embed_phase::run_embed_phase(
            result.chunk_ids_and_texts,
            &db,
            &cfg,
            tier,
            &project_root,
            args.batch_size,
            &mp,
        )
        .await?;
    } else {
        eprint_embed_skipped_notice(tier, &cfg);
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
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("index");
        cmd.arg(&args.path);
        cmd.arg("--_background-phases");
        if let Some(db_arg) = &args.db {
            cmd.args(["--db", &db_arg.to_string_lossy()]);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if cmd.spawn().is_ok() {
            return Ok(());
        }
        // Fall through and run phases 3-5 inline as fallback.
        tracing::warn!("failed to spawn background indexer; running inline");
    }

    run_phases_3_to_5(&args, &cfg, &db, &root_canonical, &db_path).await
}

/// Spawn a detached background process to run the embed phase (plus phases 3–5)
/// against the chunks the foreground run just parsed, reusing the internal
/// `--_embed-phases` mode (spelunk-oss^74). Mirrors the phases-3–5 background
/// spawn: stdio is closed so the parent regains its prompt immediately.
///
/// Returns `Ok(true)` when the subprocess was launched (caller should return),
/// `Ok(false)` when spawning failed (caller should fall back to embedding
/// inline).
fn spawn_embed_subprocess(args: &IndexArgs) -> Result<bool> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("index");
    cmd.arg(&args.path);
    cmd.arg("--_embed-phases");
    cmd.args(["--batch-size", &args.batch_size.to_string()]);
    if args.no_summaries {
        cmd.arg("--no-summaries");
    }
    if let Some(db_arg) = &args.db {
        cmd.args(["--db", &db_arg.to_string_lossy()]);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(_) => Ok(true),
        Err(e) => {
            tracing::warn!("failed to spawn detached embed process; embedding inline: {e}");
            Ok(false)
        }
    }
}

/// Embed-only entry point for the detached `--_embed-phases` subprocess: rebuild
/// the embed queue from the chunks already in the DB (no re-parse), run the
/// embed phase, then phases 3–5 (spelunk-oss^74).
async fn run_embed_phases(
    args: &IndexArgs,
    cfg: &Config,
    db: &Database,
    project_root: &std::path::Path,
    root_canonical: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    let embed_ready = matches!(tier.caps(), Some(c) if c.index_embed);
    if tier.is_server() && embed_ready {
        let chunk_ids_and_texts = parse_phase::missing_embedding_texts(db)?;
        if !chunk_ids_and_texts.is_empty() {
            let mp = MultiProgress::new();
            embed_phase::run_embed_phase(
                chunk_ids_and_texts,
                db,
                cfg,
                tier,
                project_root,
                args.batch_size,
                &mp,
            )
            .await?;
        }
    } else {
        eprint_embed_skipped_notice(tier, cfg);
    }

    run_phases_3_to_5(args, cfg, db, root_canonical, db_path).await
}

/// Build the differentiated notice lines shown when the embedding phase is
/// skipped, so an unembedded index is never a silent surprise (spelunk-oss^50
/// #5). Pure so it can be unit-tested; the four cases mirror PR A's readiness
/// contract. `server_url` is `cfg.server_url` (used only for the offline case).
fn embed_skipped_lines(
    embedder_state: Option<capability::EmbedderState>,
    server_url: Option<&str>,
) -> Vec<String> {
    use capability::EmbedderState;
    match embedder_state {
        Some(EmbedderState::Loading) => vec![
            "Note: the embedder is still warming up — chunks indexed for text/ast-grep search."
                .to_string(),
            "Re-run `spelunk index` in a moment to add embeddings (check `spelunk server status`)."
                .to_string(),
        ],
        Some(EmbedderState::Unavailable) => vec![
            "Warning: the embedder failed to load — chunks indexed for text/ast-grep search only."
                .to_string(),
            "See `spelunk server logs` for the load error, then re-run `spelunk index`."
                .to_string(),
        ],
        // Reachable server without a ready embedder for any other reason
        // (`disabled`, or an older server that never advertised `index.embed`).
        Some(_) => vec![
            "Note: this server has no embedder — chunks indexed for text/ast-grep search only."
                .to_string(),
        ],
        // Offline: no server reachable.
        None => {
            if let Some(url) = server_url {
                vec![
                    format!(
                        "Warning: spelunk-server at {url} is unreachable — skipping embedding phase."
                    ),
                    "On Windows, allow the loopback listener through Defender Firewall (accept the prompt on `spelunk server start`)."
                        .to_string(),
                    "Chunks are indexed for text/ast-grep search. Re-run `spelunk index` once the server is reachable to add embeddings."
                        .to_string(),
                ]
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
    for line in embed_skipped_lines(tier.embedder_state(), cfg.server_url.as_deref()) {
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

    // ── embed_skipped_lines: 0-chunks / offline notice (#5) ─────────────────────

    #[test]
    fn embed_skipped_loading_advises_retry() {
        let lines = embed_skipped_lines(Some(capability::EmbedderState::Loading), None);
        assert!(!lines.is_empty(), "notice must not be silent");
        let joined = lines.join("\n");
        assert!(joined.contains("warming up"));
        assert!(joined.contains("Re-run `spelunk index`"));
    }

    #[test]
    fn embed_skipped_unavailable_points_at_logs() {
        let lines = embed_skipped_lines(Some(capability::EmbedderState::Unavailable), None);
        let joined = lines.join("\n");
        assert!(joined.contains("failed to load"));
        assert!(joined.contains("spelunk server logs"));
    }

    #[test]
    fn embed_skipped_unreachable_server_mentions_firewall() {
        // Offline (no reachable server) with a configured server_url: the notice
        // names the URL and the Windows firewall cause, replacing the old silent
        // 0-chunk embed.
        let lines = embed_skipped_lines(None, Some("http://127.0.0.1:7777"));
        let joined = lines.join("\n");
        assert!(joined.contains("http://127.0.0.1:7777"));
        assert!(joined.contains("unreachable"));
        assert!(joined.contains("Firewall"));
    }

    #[test]
    fn embed_skipped_no_server_suggests_starting_one() {
        let lines = embed_skipped_lines(None, None);
        let joined = lines.join("\n");
        assert!(joined.contains("spelunk server start"));
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
                assert!(
                    !embed_skipped_lines(state, url).is_empty(),
                    "state {state:?} url {url:?} produced no notice"
                );
            }
        }
    }
}
