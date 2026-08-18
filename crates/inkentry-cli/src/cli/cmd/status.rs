use super::color::cprintln;
use anyhow::{Context, Result};
use clap::Args;

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Show stats for all registered projects, not just the current one
    #[arg(short, long)]
    pub all: bool,

    /// Brief list format (one line per project) — implies --all
    #[arg(short, long)]
    pub list: bool,

    /// Output format: text or json
    #[arg(long, default_value = "text")]
    pub format: String,
}

use crate::{
    capability::{self, Tier},
    config::Config,
    registry::{Registry, resolve_project_context},
    storage::{Database, MemoryStore, open_memory_backend},
};

/// Stable JSON schema for `inkentry status --format json` (issue #269).
///
/// All fields listed here are guaranteed additive-safe: new optional fields
/// may be added in future versions, but existing fields will not be renamed or
/// removed. Consumers must tolerate unknown fields.
///
/// Field notes:
///
/// - `version` — inkentry CLI semver string (e.g. `"0.7.0"`)
/// - `project` — absolute path to the project root (`null` if not in a registered project)
/// - `db_path` — absolute path to the SQLite index file
/// - `indexed_files` — number of source files currently in the index
/// - `total_chunks` — number of AST/text chunks derived from those files
/// - `languages` — per-language file counts, sorted by count descending;
///   files without a detected language appear as `"unknown"`
/// - `embedding_dim` — vector dimension used for semantic search (`null` when no embeddings yet)
/// - `has_semantic_search` — `true` when a server with `search.semantic` is reachable
/// - `last_indexed_at` — ISO-8601 UTC timestamp of the most recently indexed file, or `null`
/// - `memory_entries` — count of memory entries accessible from this project
/// - `memory_backend` — stable identifier for the active memory backend:
///   `"sqlite"`, `"git-notes"`, or `"remote"` (see issue #308)
///
/// Additional fields (`tier`, `mode`, `sync_pending`, `sync_last_synced_at`,
/// `server_url`, `capabilities`, `embedder_state`, `embedding_count`,
/// `embedding_pending`, `embedding_refresh_pending`, `memory_embedding_pending`,
/// `summary_scheme`, `index_rebuilt_from`, `embed_worker_alive`, `embed_tokens`,
/// `drift_candidates`,
/// `usage_7d`) are present for backward compatibility and richer tooling; treat
/// them as unstable extensions.
///
/// `memory_embedding_pending` counts memory entries with no vector — the set
/// `inkentry memory reindex` fills. `null` when there is no readable local
/// store. Worth consuming: the default search mode is hybrid, so those entries
/// still come back from the full-text half and semantic recall degrades with
/// nothing else to show for it.
///
/// `sync_pending`/`sync_last_synced_at` (ADR-037 P2) are `null` unless `mode`
/// is `"local_first"`: the outbox pending count and the local relay's last
/// successful push-ack/pull-apply time (ISO-8601 UTC), or `null` when nothing
/// has synced yet.
/// `embedder_state` mirrors the server's `/v1/health` readiness
/// (`"loading"`/`"ready"`/`"unavailable"`/`"disabled"`); it is `null` when
/// offline or when the reachable server pre-dates the readiness field.
/// `embedding_pending` is the chunk count still awaiting a first embedding
/// (coverage). `embedding_refresh_pending` is the distinct freshness signal: the
/// count of chunks that have a vector whose input changed and await an in-place
/// re-embed (`null` when none, or no index); coverage can read 100% while this
/// is non-zero, and "same query, same answer" is guaranteed only once it reaches
/// zero. `summary_scheme` is the embedding-input composition scheme the index's
/// vectors were built under (provenance), or `null`.
/// `embed_worker_alive` and `embed_tokens` describe the recorded embed
/// worker's liveness and token-weighted progress and are `null` when no embed
/// work (first-embed or refresh) is pending.
/// `index_rebuilt_from` is the schema version a rebuild discarded when this
/// build could not read the index it found, while the index it left behind is
/// still empty; `null` on an index no rebuild touched and once a reindex has
/// run. Non-null with `indexed_files: 0` is an emptied index, not an
/// unindexed project, and the two are otherwise identical.
pub async fn status(args: StatusArgs, cfg: Config) -> Result<()> {
    let fmt = crate::utils::effective_format(&args.format);

    // JSON mode: current project stats only
    if fmt == "json" {
        // ADR-067: fail closed when there is no local `.inkentry/` project rather
        // than reporting the global store as if it were this project's. The
        // scoped path also wins over any stray global `index.db`.
        let db_path = crate::config::require_project_db(&cfg.db_path, false)?;
        let tier = capability::get_tier(&cfg).await;

        let resolved = resolve_project_context(None, &cfg.db_path)?;
        let project_root: Option<String> = resolved
            .project
            .as_ref()
            .map(|p| p.root_path.display().to_string());

        let db = Database::open(&db_path)?;
        super::helpers::announce_index_rebuild(&db);
        let rebuilt_unpopulated = db.unpopulated_since_rebuild().unwrap_or(None);
        let stats = db.stats()?;
        let languages = db.language_stats().unwrap_or_default();
        let drift = db.drift_candidates(30, 10).unwrap_or_default();
        let usage = db.usage_last_7_days().unwrap_or_default();
        let mem_path = db_path.with_file_name("memory.db");
        let (memory_count, memory_backend_kind) =
            match open_memory_backend(&cfg, &mem_path, None).await.ok() {
                Some(b) => {
                    let kind = b.backend_kind();
                    let count = b.count().await.unwrap_or(0);
                    (count, kind)
                }
                None => (0, "sqlite"),
            };
        let usage_map: std::collections::HashMap<&str, i64> =
            usage.iter().map(|(c, n)| (c.as_str(), *n)).collect();

        // ADR-037 P2, item 35: additive-only JSON extensions. `null` under any
        // mode other than `local_first` (item 38: `cloud_first` has no local
        // write queue; `offline` has no sync configuration).
        //
        // Poll-and-apply BEFORE reading the pending count, not after: a poll
        // in this same call can apply push-acks/pulls that reduce (or, for a
        // pull, increase) what's actually outstanding. Reading pending first
        // would report the pre-poll count next to a same-instant
        // `last_synced_at`, understating how current the two fields actually
        // are together.
        let (sync_pending, sync_last_synced_at): (Option<i64>, Option<String>) =
            if cfg.resolve_mode() == inkentry_core::config::SyncMode::LocalFirst {
                let last_synced_at =
                    crate::cli::cmd::memory::outbox::poll_and_apply(&cfg, &mem_path)
                        .await
                        .and_then(|p| p.last_synced_at)
                        .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0))
                        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string());
                let pending = MemoryStore::open(&mem_path)
                    .ok()
                    .and_then(|s| s.pending_sync_count().ok());
                (pending, last_synced_at)
            } else {
                (None, None)
            };

        // Entries the vector index cannot see. Worth surfacing because the
        // failure is silent: the default search mode is hybrid, so full-text
        // still returns them and recall looks complete while it is not.
        let memory_embedding_pending: Option<usize> = MemoryStore::open(&mem_path)
            .ok()
            .and_then(|s| s.notes_missing_embeddings(false).ok())
            .map(|v| v.len());

        // has_semantic_search: true only when a Server tier is reachable and it
        // advertises the search.semantic capability.
        let has_semantic_search = matches!(
            tier,
            Tier::Server { caps, .. } if caps.search_semantic
        );

        // ISO-8601 UTC timestamp for last_indexed_at
        let last_indexed_at: Option<String> = stats.last_indexed.and_then(|ts| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        });

        // Embedding dimension: use the compile-time constant when embeddings
        // exist; null when the index has no embeddings yet.
        let embedding_dim: Option<u64> = if stats.embedding_count > 0 {
            Some(crate::embeddings::EMBEDDING_DIM as u64)
        } else {
            None
        };

        let (tier_str, tier_url, caps_json) = match tier {
            Tier::Offline => ("offline", serde_json::Value::Null, serde_json::Value::Null),
            Tier::Server { url, caps, .. } => (
                "server",
                serde_json::Value::String(url.clone()),
                serde_json::to_value(caps).unwrap_or(serde_json::Value::Null),
            ),
        };

        // Server-side embedder readiness. `null` when offline
        // or when talking to a server that pre-dates the readiness field.
        let embedder_state_json: serde_json::Value = match tier.embedder_state() {
            Some(capability::EmbedderState::Unknown) | None => serde_json::Value::Null,
            Some(s) => serde_json::Value::String(s.as_str().to_string()),
        };

        // Embed-state extensions: worker liveness is read from the recorded
        // pid (never inferred from counts) and only meaningful while work is
        // pending; token sums carry their own denominators.
        //
        // Freshness is a second, orthogonal signal to coverage: `pending_chunks`
        // counts chunks with no vector at all (coverage), `refresh_pending`
        // counts chunks that have a vector whose input changed and await an
        // in-place re-embed (freshness). A tier-3 drain or a post-migration
        // re-embed can be live while coverage reads 100%, so worker liveness
        // must consider both — otherwise `status` would say "not running" during
        // a real refresh drain.
        let pending_chunks = stats.chunk_count - stats.embedding_count;
        let refresh_pending = db.refresh_pending_count().unwrap_or(0);
        let (embed_worker_alive_json, embed_tokens_json) =
            if pending_chunks > 0 || refresh_pending > 0 {
                let alive = super::embed_worker::worker_liveness(&db_path)
                    == super::embed_worker::WorkerLiveness::Alive;
                let tokens = db
                    .embed_token_stats()
                    .ok()
                    .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null))
                    .unwrap_or(serde_json::Value::Null);
                (serde_json::json!(alive), tokens)
            } else {
                (serde_json::Value::Null, serde_json::Value::Null)
            };
        // `null` when nothing awaits re-embed (or no index), always carrying its
        // own count when non-zero.
        let embedding_refresh_pending_json = if refresh_pending > 0 {
            serde_json::json!(refresh_pending)
        } else {
            serde_json::Value::Null
        };
        let summary_scheme_json = match db.summary_scheme() {
            Ok(Some(s)) => serde_json::Value::String(s),
            _ => serde_json::Value::Null,
        };

        // Serialize languages as [{name, file_count}, ...]
        let languages_json: Vec<serde_json::Value> = languages
            .iter()
            .map(|l| {
                serde_json::json!({
                    "name": l.name,
                    "file_count": l.file_count
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                // ── Stable schema (issue #269) ────────────────────────────────
                "version": env!("CARGO_PKG_VERSION"),
                "project": project_root,
                "db_path": db_path.display().to_string(),
                "indexed_files": stats.file_count,
                "file_count": stats.file_count,  // alias for backward compat
                "total_chunks": stats.chunk_count,
                "languages": languages_json,
                "embedding_dim": embedding_dim,
                "has_semantic_search": has_semantic_search,
                "last_indexed_at": last_indexed_at,
                "memory_entries": memory_count,
                "memory_backend": memory_backend_kind,
                // ── Extensions (backward-compat, may change) ─────────────────
                "tier": tier_str,
                "mode": cfg.resolve_mode().as_str(),
                "memory_embedding_pending": memory_embedding_pending,
                "sync_pending": sync_pending,
                "sync_last_synced_at": sync_last_synced_at,
                "server_url": tier_url,
                "capabilities": caps_json,
                "embedder_state": embedder_state_json,
                "embedding_count": stats.embedding_count,
                "embedding_pending": pending_chunks,
                "embedding_refresh_pending": embedding_refresh_pending_json,
                "summary_scheme": summary_scheme_json,
                "index_rebuilt_from": rebuilt_unpopulated,
                "embed_worker_alive": embed_worker_alive_json,
                "embed_tokens": embed_tokens_json,
                "drift_candidates": drift,
                "usage_7d": {
                    "search": usage_map.get("search").copied().unwrap_or(0),
                    "memory_search": usage_map.get("memory search").copied().unwrap_or(0),
                }
            }))?
        );
        return Ok(());
    }

    // --list implies --all
    let show_all = args.all || args.list;

    if show_all {
        let reg = Registry::open().context("opening registry")?;
        let projects = reg.all_projects()?;

        if projects.is_empty() {
            println!("No projects registered. Run `inkentry index <path>` to get started.");
            return Ok(());
        }

        if args.list {
            // Brief table: one line per project
            println!(
                "{:<6}  {:<8}  {:<10}  {:<10}  Root",
                "Files", "Chunks", "Embeddings", "Registered"
            );
            println!("{}", "─".repeat(70));
            for p in &projects {
                let stats = Database::open(&p.db_path).and_then(|db| db.stats()).ok();
                let (files, chunks, embeddings) = stats
                    .map(|s| (s.file_count, s.chunk_count, s.embedding_count))
                    .unwrap_or((0, 0, 0));
                let exists = if p.root_path.exists() {
                    ""
                } else {
                    " [missing]"
                };
                println!(
                    "{:<6}  {:<8}  {:<10}  {:<10}  {}{}",
                    files,
                    chunks,
                    embeddings,
                    format_age(p.registered_at),
                    p.root_path.display(),
                    exists
                );
            }
        } else {
            // Detailed view per project
            for p in &projects {
                cprintln!("\x1b[1m{}\x1b[0m", p.root_path.display());
                if !p.root_path.exists() {
                    cprintln!("  \x1b[31m[root path missing from disk]\x1b[0m");
                }
                println!("  DB: {}", p.db_path.display());
                println!("  Registered: {}", format_age(p.registered_at));
                match Database::open(&p.db_path).and_then(|db| db.stats()) {
                    Ok(s) => {
                        println!(
                            "  Files: {}  Chunks: {}  Embeddings: {}",
                            s.file_count, s.chunk_count, s.embedding_count
                        );
                        if let Some(ts) = s.last_indexed {
                            println!("  Last indexed: {}", format_age(ts));
                        }
                    }
                    Err(_) => cprintln!("  \x1b[2m(no index yet)\x1b[0m"),
                }
                let deps = reg.get_deps(p.id)?;
                if !deps.is_empty() {
                    println!("  Depends on:");
                    for dep in &deps {
                        println!("    → {}", dep.root_path.display());
                    }
                }
                println!();
            }
        }
        return Ok(());
    }

    // Current project only.
    // ADR-067: fail closed when there is no local `.inkentry/` project rather than
    // describing the global store. The scoped path also wins over a stray global
    // `index.db`.
    let db_path = match crate::config::require_project_db(&cfg.db_path, false) {
        Ok(p) => p,
        Err(_) => {
            println!("No inkentry project here. Run `inkentry init` first.");
            return Ok(());
        }
    };
    let tier = capability::get_tier(&cfg).await;

    let resolved = resolve_project_context(None, &cfg.db_path)?;

    if !db_path.exists() {
        println!("No index found for the current directory (checked parents too).");
        println!("Run `inkentry index <path>` to create one.");
        return Ok(());
    }

    let db = Database::open(&db_path)?;
    super::helpers::announce_index_rebuild(&db);
    let s = db.stats()?;

    // ── Memory backend (single truthful line from the resolved backend, ADR-067 D3) ──
    let mem_path_text = db_path.with_file_name("memory.db");
    let mem_label = match open_memory_backend(&cfg, &mem_path_text, None).await {
        Ok(b) => memory_backend_label(b.backend_kind()).to_string(),
        Err(_) => "unavailable".to_string(),
    };

    // ── Capability tier section ───────────────────────────────────────────────
    print_tier_section(tier, &cfg, &mem_label, &mem_path_text).await;

    if let Some(p) = &resolved.project {
        cprintln!("Project: \x1b[1m{}\x1b[0m", p.root_path.display());
    }
    println!("Index:      {}", db_path.display());
    println!("Files:      {}", s.file_count);
    println!("Chunks:     {}", s.chunk_count);
    println!("Embeddings: {}", s.embedding_count);
    if let Some(line) = rebuilt_line(db.unpopulated_since_rebuild().unwrap_or(None)) {
        cprintln!("{line}");
    }
    // Surface remaining embed work from what the process actually knows: the
    // worker's recorded pid liveness (not a guess from two integers), chunk
    // coverage, and the token-weighted work fraction. Coverage and progress
    // are two measures in two units under two names; on a real repo they
    // diverge by 2x and that divergence is the fact being reported.
    if s.chunk_count > s.embedding_count {
        let tokens = db.embed_token_stats().ok();
        let worker = super::embed_worker::worker_liveness(&db_path);
        let worker_alive = worker == super::embed_worker::WorkerLiveness::Alive;
        let eta = match (&tokens, worker_alive) {
            (Some(t), true) => super::embed_worker::worker_eta(&db_path, t.pending_tokens),
            _ => None,
        };
        let embedder_unavailable = matches!(
            tier.embedder_state(),
            Some(capability::EmbedderState::Unavailable)
        );
        if let Some(line) = embedding_state_line(
            worker_alive,
            embedder_unavailable,
            s.chunk_count,
            s.embedding_count,
            tokens.as_ref().map(|t| t.total_tokens).unwrap_or(0),
            tokens.as_ref().map(|t| t.pending_tokens).unwrap_or(0),
            eta,
        ) {
            cprintln!("{line}");
        }
        if let Some(line) = embed_threads_line(tier.server_limits().and_then(|l| l.embed_threads)) {
            cprintln!("{line}");
        }
    }
    if let Some(line) = memory_embedding_line(&mem_path_text) {
        cprintln!("{line}");
    }
    if let Some(ts) = s.last_indexed {
        println!("Last index: {}", format_age(ts));
    }

    // Show dependencies
    if !resolved.deps.is_empty() {
        println!("\nDependencies:");
        for dep in &resolved.deps {
            let dep_stats = Database::open(&dep.db_path).and_then(|db| db.stats()).ok();
            let summary = dep_stats
                .map(|s| format!("{} files, {} chunks", s.file_count, s.chunk_count))
                .unwrap_or_else(|| "not indexed".to_string());
            println!("  → {}  ({})", dep.root_path.display(), summary);
        }
    }

    // Drift signals: files that haven't changed while the project has evolved
    let drift = db.drift_candidates(30, 5).unwrap_or_default();
    if !drift.is_empty() {
        cprintln!("\n\x1b[33mDrift signals\x1b[0m  (unchanged while project evolved):");
        println!("  {:<6}  {:<8}  File", "Days", "Callers");
        println!("  {}", "─".repeat(60));
        for d in &drift {
            let callers = if d.caller_count > 0 {
                format!("{}", d.caller_count)
            } else {
                "—".to_string()
            };
            println!("  {:<6}  {:<8}  {}", d.days_behind, callers, d.path);
        }
        cprintln!(
            "  \x1b[2mRun `inkentry search \"<topic>\"` to check if these are still relevant.\x1b[0m"
        );
    }

    // Usage summary (last 7 days)
    let usage = db.usage_last_7_days().unwrap_or_default();
    let total: i64 = usage.iter().map(|(_, n)| n).sum();
    if total > 0 {
        const COMMANDS: &[&str] = &["search", "memory search"];
        println!("\nUsage (last 7 days)");
        for cmd in COMMANDS {
            let count = usage
                .iter()
                .find(|(c, _)| c == cmd)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            if count > 0 {
                println!("  {:<16}  {} calls", cmd, count);
            }
        }
    }

    Ok(())
}

/// `mem_label` is the resolved memory line (ADR-067 D3): derived from the opened
/// backend's `backend_kind()`, never inferred from the capability tier.
/// `mem_path` is the project's `memory.db` path, threaded through to
/// [`sync_mode_line`] for the ADR-037 P2 pending/last-synced extension.
async fn print_tier_section(
    tier: &Tier,
    cfg: &Config,
    mem_label: &str,
    mem_path: &std::path::Path,
) {
    match tier {
        Tier::Offline => {
            let server_hint = if cfg.server_url.is_some() {
                match capability::explicit_probe_failure() {
                    Some(capability::ConnFailure::Tls(cause)) => format!("  [tls: {cause}]"),
                    _ => "  [unreachable]".to_string(),
                }
            } else {
                "  [set server_url to enable semantic search]".to_string()
            };
            cprintln!("Capability tier:  \x1b[33mOffline\x1b[0m");
            if let Some(line) = sync_mode_line(cfg, mem_path).await {
                println!("{line}");
            }
            println!("  search          text{server_hint}");
            println!("  memory          {mem_label}");
        }
        Tier::Server {
            url,
            caps,
            auto_discovered,
            embedder_state,
            ..
        } => {
            let url_label = if *auto_discovered {
                format!("{url}  \x1b[2m(local, auto)\x1b[0m")
            } else {
                url.clone()
            };
            cprintln!("Capability tier:  \x1b[32mServer\x1b[0m  \x1b[2m({url_label})\x1b[0m");
            if let Some(line) = sync_mode_line(cfg, mem_path).await {
                println!("{line}");
            }
            let search_label = if caps.search_semantic {
                "text + semantic"
            } else {
                "text"
            };
            println!("  search          {search_label}");
            // Embedder readiness: explain *why* semantic search isn't in the
            // search line yet when the server is up but the model isn't ready.
            // Log hints must point at the probed server: `inkentry server logs`
            // reads the local daemon's logs, which are the wrong place when the
            // failing embedder lives on an explicit remote server_url.
            let remote_url = (!*auto_discovered).then_some(url.as_str());
            if let Some(line) = embedder_status_line(embedder_state, remote_url) {
                cprintln!("{line}");
            }
            println!("  memory          {mem_label}");
        }
    }
    println!();
}

/// The `mode` line for `inkentry status`: a neutral one-word sync-mode
/// indicator. `None` on the solo default (no `server_url`, no explicit mode):
/// there is no sync configuration to surface. No call to action: the background
/// reconciler owns convergence, so status must not pre-teach a manual `inkentry
/// sync` workflow.
///
/// ADR-037 P2: under `local_first`, this same line additionally carries a
/// quiet pending-count / last-synced clause (item 31) — never a second,
/// separate line. `cloud_first` and `offline` render the bare mode word only
/// (items 37/38): `cloud_first` has no local write queue to report on, and
/// `offline` has no sync configuration to poll.
async fn sync_mode_line(cfg: &Config, mem_path: &std::path::Path) -> Option<String> {
    if cfg.server_url.is_none() && cfg.mode.is_none() {
        return None;
    }
    let mode = cfg.resolve_mode();
    let mut line = format!("  {:<16}{}", "mode", mode.as_str());
    if mode == inkentry_core::config::SyncMode::LocalFirst
        && let Some(suffix) = sync_status_suffix(cfg, mem_path).await
    {
        line.push_str(&suffix);
    }
    Some(line)
}

/// The pending-count / last-synced clause appended to [`sync_mode_line`]
/// under `local_first`. `None` when there is nothing worth reporting yet (no
/// pending rows and no recorded sync ever) — a fresh project stays silent
/// rather than printing a hollow "up to date".
///
/// Polls and applies the local relay's buffered state first (item 33: that
/// state lives in the separate, longer-running `inkentry-server` process, so a
/// fresh poll here is what makes "last synced" current on this invocation),
/// and only then reads the pending count: a poll applied in this same call
/// can itself change what's outstanding, so reading pending first would
/// report the pre-poll count alongside a same-instant "last synced", making
/// the two fields inconsistent with each other for this one invocation. The
/// poll itself is best-effort: with no local relay reachable, `pending` still
/// comes from the always-available local `pending_sync_count` and "last
/// synced" is simply omitted.
async fn sync_status_suffix(cfg: &Config, mem_path: &std::path::Path) -> Option<String> {
    let store = MemoryStore::open(mem_path).ok()?;
    let poll = crate::cli::cmd::memory::outbox::poll_and_apply(cfg, mem_path).await;
    let pending = store.pending_sync_count().ok()?;
    let last_synced_at = poll.as_ref().and_then(|p| p.last_synced_at);
    // item 19: a relay-side failure (e.g. an expired-and-unrefreshable bearer)
    // surfaces here rather than crashing or silently dropping.
    let last_error = poll.and_then(|p| p.last_error);

    if pending == 0 && last_synced_at.is_none() && last_error.is_none() {
        return None;
    }
    let pending_clause = if pending > 0 {
        format!("{pending} pending")
    } else {
        "up to date".to_string()
    };
    let mut clause = match last_synced_at {
        Some(ts) => format!("{pending_clause}, last synced {}", format_age(ts)),
        None => pending_clause,
    };
    if let Some(err) = last_error {
        let truncated: String = err.chars().take(80).collect();
        clause.push_str(&format!(", sync error: {truncated}"));
    }
    Some(format!("  \u{b7}  {clause}"))
}

/// Human-readable label for a resolved memory `backend_kind()` (ADR-067 D3).
/// The parenthetical is derived from the backend kind, not the capability tier.
fn memory_backend_label(kind: &str) -> &str {
    match kind {
        "sqlite" => "sqlite (local)",
        "git-notes" => "git-notes (local)",
        "remote" => "remote (server)",
        other => other,
    }
}

/// Render the `embedder` line for `inkentry status` (text mode) from the
/// server-side readiness state, or `None` when there is nothing useful to show
/// (an older server that never reported readiness). Pure so it can be unit
/// tested without capturing stdout.
///
/// `remote_url` is `Some` when the probed server came from an explicit
/// `server_url` (not loopback auto-discovery). The failure-hint must then point
/// at that server's own logs: `inkentry server logs` only reads the local
/// daemon's log file, so with a healthy local daemon it shows clean logs for a
/// failure that lives elsewhere.
fn embedder_status_line(
    state: &capability::EmbedderState,
    remote_url: Option<&str>,
) -> Option<String> {
    use capability::EmbedderState;
    let line = match state {
        EmbedderState::Loading => {
            "  embedder        \x1b[33mloading\x1b[0m  [model warming up — retry shortly]"
                .to_string()
        }
        EmbedderState::Unavailable => match remote_url {
            Some(url) => format!(
                "  embedder        \x1b[31munavailable\x1b[0m  [model failed to load on team \
                 server {url}; check that server's own logs]"
            ),
            None => "  embedder        \x1b[31munavailable\x1b[0m  [model failed to load; \
                 see `inkentry server logs`]"
                .to_string(),
        },
        EmbedderState::Ready => "  embedder        ready".to_string(),
        EmbedderState::Disabled => {
            "  embedder        disabled  [server built without a native embedder]".to_string()
        }
        // Older server without the readiness field: stay quiet rather than
        // print a confusing "unknown".
        EmbedderState::Unknown => return None,
    };
    Some(line)
}

/// Integer percentage with an explicit denominator; `None` when the
/// denominator is empty (the caller omits the clause rather than printing a
/// made-up number).
fn labelled_pct(done: i64, total: i64) -> Option<u64> {
    (done.max(0) as u64)
        .saturating_mul(100)
        .checked_div(u64::try_from(total).ok().filter(|t| *t > 0)?)
}

/// `~54 min left` style rendering for the status ETA.
fn humanize_eta(eta: std::time::Duration) -> String {
    let secs = eta.as_secs();
    if secs < 60 {
        format!("~{secs}s left")
    } else if secs < 3600 {
        format!("~{} min left", secs.div_ceil(60))
    } else {
        format!("~{}h{:02}m left", secs / 3600, (secs % 3600) / 60)
    }
}

/// Render the embedding-state line for `inkentry status` when the index has
/// more chunks than embeddings. Pure so the state matrix is unit testable.
///
/// The line reports what the process knows, never a guess:
/// - a live recorded worker: `Embedding in progress`
/// - no live worker but pending work: `Embedding incomplete` plus the resume
///   command (or, when the embedder is unavailable, a pointer at the server
///   logs instead, since resuming cannot help until the server is fixed)
///
/// Two measures, two units, two names: `searchable` is chunk coverage (what
/// KNN can see), `of work done` is token-weighted progress (how much of the
/// wait is behind you). They are supposed to diverge; a single unlabelled
/// percentage serving both questions is the defect this replaces. On an index
/// whose token counts are not backfilled (total 0) the work clause is omitted
/// rather than fabricated.
/// One line naming memory entries the vector index cannot see, and the command
/// that fixes it. `None` when there are none, or when there is no local store.
///
/// Worth a line of its own rather than a footnote: the default search mode is
/// hybrid, so these entries are still returned by the full-text half. Recall
/// degrades with nothing to show for it, which is worse than an empty result.
fn memory_embedding_line(mem_path: &std::path::Path) -> Option<String> {
    if !mem_path.exists() {
        return None;
    }
    let pending = MemoryStore::open(mem_path)
        .ok()?
        .notes_missing_embeddings(false)
        .ok()?
        .len();
    if pending == 0 {
        return None;
    }
    Some(format!(
        "\x1b[33m[Memory: {pending} entr{} not in semantic search; \
         run 'inkentry memory reindex']\x1b[0m",
        if pending == 1 { "y" } else { "ies" }
    ))
}

/// One line saying the zeros above are an index this build emptied, not a
/// project nobody has indexed. `None` when no rebuild is outstanding.
///
/// The two states print identical counts, so without this the reader has to
/// already know a rebuild happened to read the zeros as anything but "nothing
/// here yet".
fn rebuilt_line(rebuilt_from: Option<i32>) -> Option<String> {
    let found = rebuilt_from?;
    Some(format!(
        "\x1b[33m[Index: emptied by a rebuild from {}, not yet reindexed; \
         run 'inkentry index .']\x1b[0m",
        super::helpers::replaced_schema(found)
    ))
}

fn embedding_state_line(
    worker_alive: bool,
    embedder_unavailable: bool,
    chunk_count: i64,
    embedding_count: i64,
    total_tokens: i64,
    pending_tokens: i64,
    eta: Option<std::time::Duration>,
) -> Option<String> {
    if chunk_count <= 0 || embedding_count >= chunk_count {
        return None;
    }
    let coverage = labelled_pct(embedding_count, chunk_count).unwrap_or(0);
    let searchable = format!("searchable {embedding_count}/{chunk_count} chunks ({coverage}%)");

    let mut progress = match labelled_pct(
        (total_tokens - pending_tokens).clamp(0, total_tokens),
        total_tokens,
    ) {
        Some(work) => format!("{work}% of work done"),
        None => "work remaining unknown (run `inkentry index --recount` to backfill token counts)"
            .to_string(),
    };
    if worker_alive && let Some(eta) = eta {
        progress = format!("{progress}, {}", humanize_eta(eta));
    }

    Some(if worker_alive {
        format!("  \x1b[33mEmbedding in progress\x1b[0m   {searchable}  \u{00b7}  {progress}")
    } else if embedder_unavailable {
        format!(
            "  \x1b[33mEmbedding incomplete\x1b[0m   {searchable}  \u{00b7}  {progress}; \
             the embedder is unavailable, see `inkentry server logs`"
        )
    } else {
        format!(
            "  \x1b[33mEmbedding incomplete\x1b[0m   {searchable}  \u{00b7}  {progress}; \
             resume with `inkentry index .`"
        )
    })
}

/// The server's embed thread budget, rendered only when it is 1. That is the
/// case where a first index takes hours instead of minutes, and the override
/// that fixes it is otherwise reachable only by reading the server log.
fn embed_threads_line(embed_threads: Option<usize>) -> Option<String> {
    (embed_threads? == 1).then(|| {
        "  \x1b[2mThe server is embedding single-threaded; set INKENTRY_EMBED_THREADS=<n> \
         and restart it (`inkentry server stop`) if this host can spare the cores.\x1b[0m"
            .to_string()
    })
}

pub(crate) fn format_age(unix_ts: i64) -> String {
    let Some(then) = chrono::DateTime::<chrono::Utc>::from_timestamp(unix_ts, 0) else {
        return "unknown".to_string();
    };
    let elapsed = chrono::Utc::now().signed_duration_since(then);
    let secs = elapsed.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::EmbedderState;

    // ── embedder_status_line: `inkentry status` rendering of each state ──────────

    #[test]
    fn embedder_line_loading_advises_warmup() {
        let line =
            embedder_status_line(&EmbedderState::Loading, None).expect("loading renders a line");
        assert!(line.contains("loading"));
        assert!(line.contains("warming up"));
    }

    #[test]
    fn embedder_line_unavailable_loopback_points_at_local_logs() {
        // Loopback auto-discovery: the failing embedder IS the local daemon, so
        // `inkentry server logs` is the right place to look.
        let line = embedder_status_line(&EmbedderState::Unavailable, None)
            .expect("unavailable renders a line");
        assert!(line.contains("unavailable"));
        assert!(line.contains("failed to load"));
        assert!(line.contains("inkentry server logs"));
    }

    #[test]
    fn embedder_line_unavailable_remote_points_at_that_server_never_local_logs() {
        // Explicit server_url: `inkentry server logs` reads the LOCAL daemon's
        // log, which is clean when the failure lives on the team server. The
        // hint must name the probed server instead.
        let line = embedder_status_line(
            &EmbedderState::Unavailable,
            Some("https://team.example:7777"),
        )
        .expect("unavailable renders a line");
        assert!(line.contains("unavailable"));
        assert!(line.contains("https://team.example:7777"), "got: {line}");
        assert!(
            !line.contains("inkentry server logs"),
            "must not point a remote failure at local logs: {line}"
        );
    }

    #[test]
    fn embedder_line_ready_is_plain() {
        let line = embedder_status_line(&EmbedderState::Ready, None).expect("ready renders a line");
        assert!(line.contains("ready"));
    }

    #[test]
    fn embedder_line_disabled_notes_no_native_embedder() {
        // `Disabled` now means only one thing: this server binary was built
        // without the `embed-native` feature. The external-relocation
        // backend this line used to describe no longer exists, so the line
        // must not claim it does.
        let line =
            embedder_status_line(&EmbedderState::Disabled, None).expect("disabled renders a line");
        assert!(line.contains("disabled"));
        assert!(
            !line.contains("external"),
            "the external embedding backend concept no longer exists: {line}"
        );
        assert!(line.contains("native embedder"), "got: {line}");
    }

    #[test]
    fn embedder_line_unknown_renders_nothing() {
        // Older server without the readiness field: no line rather than a
        // confusing "unknown".
        assert!(embedder_status_line(&EmbedderState::Unknown, None).is_none());
        assert!(embedder_status_line(&EmbedderState::Unknown, Some("https://t:1")).is_none());
    }

    // ── sync_mode_line: "local by design" vs "local because broken" ─────────────

    fn clear_no_server_env() {
        // SAFETY: serialised via #[serial] on every test that calls this, so no
        // other test reads/writes this env var concurrently.
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
    }

    /// A path that opens as an empty, ephemeral SQLite DB — fine for the mode
    /// branches that never reach `sync_status_suffix` (cloud_first / offline
    /// / no-config), which never actually query it.
    fn unused_mem_path() -> std::path::PathBuf {
        std::path::PathBuf::from(":memory:")
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn mode_line_absent_on_solo_default() {
        clear_no_server_env();
        // No server_url, no explicit mode: nothing to explain, output unchanged.
        let cfg = crate::config::Config::default();
        assert!(sync_mode_line(&cfg, &unused_mem_path()).await.is_none());
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn mode_line_local_first_is_neutral_mode_word_without_call_to_action() {
        clear_no_server_env();
        // Isolate from any real local inkentry-server daemon on this machine:
        // the local_first branch polls the local relay via `INKENTRY_STATE_DIR`.
        let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
        let tmp_state = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("INKENTRY_STATE_DIR", tmp_state.path()) };

        let cfg = crate::config::Config {
            server_url: Some("https://team.example:7777".to_string()),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg, &unused_mem_path())
            .await
            .expect("server_url set renders a mode line");

        // SAFETY: serialised via #[serial(server_state_dir_env)] against every
        // other test touching this var.
        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                None => std::env::remove_var("INKENTRY_STATE_DIR"),
            }
        }

        assert!(line.contains("local_first"), "got: {line}");
        // Neutral indicator only: no manual-sync imperative (the background
        // reconciler owns convergence).
        assert!(!line.contains("inkentry sync"), "got: {line}");
        // item 32: a fresh, empty memory.db with nothing pending and nothing
        // ever synced renders no suffix clause at all (no hollow "up to date").
        assert!(!line.contains("pending"), "got: {line}");
    }

    // ── items 31/32: pending-count clause, purely from the local outbox ─────
    // (no relay reachable — the clause must not depend on it for `pending`,
    // only for `last synced`).

    // The relay only connects to team targets local configuration declares;
    // these tests stand in for that config by declaring their mock team server.
    fn relay_declaring(
        server_url: &str,
        project_id: &str,
    ) -> inkentry_server::relay::RelayRegistry {
        inkentry_server::relay::RelayRegistry::new(inkentry_server::relay::RelayPolicy::allowing(
            vec![inkentry_core::config::TeamTarget {
                server_url: server_url.to_string(),
                project_id: project_id.to_string(),
                server_ca: None,
            }],
        ))
    }

    fn register_sqlite_vec_for_status_tests() {
        use std::sync::OnceLock;
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

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn mode_line_local_first_shows_pending_count_from_local_outbox_alone() {
        clear_no_server_env();
        register_sqlite_vec_for_status_tests();
        let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
        // Empty state dir: no local relay reachable, so this must come from
        // `pending_sync_count()` alone, never a poll.
        let tmp_state = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("INKENTRY_STATE_DIR", tmp_state.path()) };

        let tmp_mem = tempfile::TempDir::new().unwrap();
        let mem_path = tmp_mem.path().join("memory.db");
        {
            let store = crate::storage::MemoryStore::open(&mem_path).unwrap();
            store
                .add_note("decision", "One", "b", &[], &[], None, None)
                .unwrap();
            store
                .add_note("decision", "Two", "b", &[], &[], None, None)
                .unwrap();
        }

        let cfg = crate::config::Config {
            server_url: Some("https://team.example:7777".to_string()),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg, &mem_path).await.expect("mode line");

        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                None => std::env::remove_var("INKENTRY_STATE_DIR"),
            }
        }

        assert!(line.contains("local_first"), "got: {line}");
        assert!(line.contains("2 pending"), "got: {line}");
        // item 36: never a manual-action suggestion, even with pending rows.
        assert!(!line.contains("inkentry sync"), "got: {line}");
        // item 33: nothing has synced yet (no relay reachable) — no "last
        // synced" clause fabricated.
        assert!(!line.contains("last synced"), "got: {line}");
    }

    // ── item 33: "last synced" renders once the relay has actually synced ──

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn mode_line_shows_last_synced_after_a_real_relay_round_trip() {
        clear_no_server_env();
        register_sqlite_vec_for_status_tests();

        let team_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/projects/proj/memory/batch"))
            .respond_with(
                wiremock::ResponseTemplate::new(207).set_body_json(serde_json::json!({
                    "created": 1, "skipped": 0, "failed": 0, "results": []
                })),
            )
            .mount(&team_server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/projects/proj/memory/since"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "entries": [], "count": 0
                })),
            )
            .mount(&team_server)
            .await;

        // A real inkentry-server, in its LOCAL relay role, on an ephemeral port.
        let db_dir = tempfile::TempDir::new().unwrap();
        let db =
            inkentry_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
                .unwrap();
        let instance_id = db.get_or_create_instance_id().unwrap();
        let state = inkentry_server::AppState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
            auth: std::sync::Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
            conflict_threshold: inkentry_server::default_conflict_threshold(),
            embedder: inkentry_server::EmbedderSlot::disabled(),
            embed_admission: inkentry_server::EmbedAdmission::new(
                inkentry_server::EMBED_QUEUE_CAPACITY,
                inkentry_server::EMBED_BUSY_RETRY_AFTER_SECS,
            ),
            embed_threads: 4,
            llm: None,
            max_tokens_ceiling: 8192,
            rate_limiter: std::sync::Arc::new(inkentry_server::rate_limiter::RateLimiter::new(
                1000, 60,
            )),
            instance_id,
            started_by: None,
            trusted_proxies: Default::default(),
            relay: relay_declaring(&team_server.uri(), "proj"),
            repair_signal: inkentry_server::repair::RepairSignal::new(),
        };
        let app = inkentry_server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
        let tmp_state = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("INKENTRY_STATE_DIR", tmp_state.path()) };
        std::fs::write(
            tmp_state.path().join("server.port"),
            format!("{relay_port}\n"),
        )
        .unwrap();

        let tmp_mem = tempfile::TempDir::new().unwrap();
        let mem_path = tmp_mem.path().join("memory.db");
        {
            let store = crate::storage::MemoryStore::open(&mem_path).unwrap();
            store
                .add_note("decision", "One", "b", &[], &[], None, None)
                .unwrap();
        }

        let cfg = crate::config::Config {
            server_url: Some(team_server.uri()),
            project_id: Some("proj".to_string()),
            ..Default::default()
        };

        // Poll until the relay has actually synced (its own detached push
        // task needs a moment), then read the status line.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut line = None;
        while std::time::Instant::now() < deadline {
            let candidate = sync_mode_line(&cfg, &mem_path).await;
            if candidate
                .as_deref()
                .is_some_and(|l| l.contains("last synced"))
            {
                line = candidate;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }

        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                None => std::env::remove_var("INKENTRY_STATE_DIR"),
            }
        }

        let line = line.expect("status line must show 'last synced' after the relay syncs");
        assert!(line.contains("local_first"), "got: {line}");
        assert!(line.contains("last synced"), "got: {line}");
        assert!(!line.contains("inkentry sync"), "got: {line}");
    }

    // ── pending must reflect the SAME call's own poll, not the pre-poll state ─
    //
    // `sync_status_suffix` polls (which can apply a push-ack) and reads
    // `pending_sync_count` in the same call. Reading pending before the poll
    // would report a stale, pre-apply count next to a same-instant "last
    // synced" — e.g. "1 pending, last synced 0s ago" for a row that this very
    // call just finished stamping. This pins the fix: the first call whose
    // poll actually lands the ack must already show the post-apply count.

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn mode_line_pending_count_reflects_the_same_calls_own_poll_not_the_stale_pre_poll_state()
    {
        clear_no_server_env();
        register_sqlite_vec_for_status_tests();

        let team_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/projects/proj/memory/since"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "entries": [], "count": 0
                })),
            )
            .mount(&team_server)
            .await;

        // A real inkentry-server, in its LOCAL relay role, on an ephemeral port.
        let db_dir = tempfile::TempDir::new().unwrap();
        let db =
            inkentry_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
                .unwrap();
        let instance_id = db.get_or_create_instance_id().unwrap();
        let state = inkentry_server::AppState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
            auth: std::sync::Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
            conflict_threshold: inkentry_server::default_conflict_threshold(),
            embedder: inkentry_server::EmbedderSlot::disabled(),
            embed_admission: inkentry_server::EmbedAdmission::new(
                inkentry_server::EMBED_QUEUE_CAPACITY,
                inkentry_server::EMBED_BUSY_RETRY_AFTER_SECS,
            ),
            embed_threads: 4,
            llm: None,
            max_tokens_ceiling: 8192,
            rate_limiter: std::sync::Arc::new(inkentry_server::rate_limiter::RateLimiter::new(
                1000, 60,
            )),
            instance_id,
            started_by: None,
            trusted_proxies: Default::default(),
            relay: relay_declaring(&team_server.uri(), "proj"),
            repair_signal: inkentry_server::repair::RepairSignal::new(),
        };
        let app = inkentry_server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
        let tmp_state = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("INKENTRY_STATE_DIR", tmp_state.path()) };
        std::fs::write(
            tmp_state.path().join("server.port"),
            format!("{relay_port}\n"),
        )
        .unwrap();

        let tmp_mem = tempfile::TempDir::new().unwrap();
        let mem_path = tmp_mem.path().join("memory.db");
        let uuid = {
            let store = crate::storage::MemoryStore::open(&mem_path).unwrap();
            store
                .add_note("decision", "One", "b", &[], &[], None, None)
                .unwrap();
            store.rows_for_sync(false).unwrap()[0].id.to_string()
        };
        // Mounted with this note's actual id so the push handler's ack
        // round-trips onto the real row (matching `poll_and_apply`'s
        // `has_note` lookup).
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/projects/proj/memory/batch"))
            .respond_with(
                wiremock::ResponseTemplate::new(207).set_body_json(serde_json::json!({
                    "created": 1, "skipped": 0, "failed": 0,
                    "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
                })),
            )
            .mount(&team_server)
            .await;

        let cfg = crate::config::Config {
            server_url: Some(team_server.uri()),
            project_id: Some("proj".to_string()),
            ..Default::default()
        };

        // Poll `sync_mode_line` directly (not through a prior nudge) so the
        // very first call that observes "last synced" is also the call whose
        // own poll applied the ack: exactly the window the ordering bug lived
        // in.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut line = None;
        while std::time::Instant::now() < deadline {
            let candidate = sync_mode_line(&cfg, &mem_path).await;
            if candidate
                .as_deref()
                .is_some_and(|l| l.contains("last synced"))
            {
                line = candidate;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                None => std::env::remove_var("INKENTRY_STATE_DIR"),
            }
        }

        let line = line.expect("status line must show 'last synced' after the relay syncs");
        assert!(
            line.contains("up to date"),
            "the call that first reports 'last synced' must already reflect its OWN \
             poll's apply, not a stale pre-poll pending count: got {line}"
        );
        assert!(
            !line.contains("1 pending"),
            "must never show a pending count for a row this same call just stamped: got {line}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn mode_line_cloud_first_is_neutral_mode_word() {
        clear_no_server_env();
        let cfg = crate::config::Config {
            server_url: Some("https://team.example:7777".to_string()),
            mode: Some(crate::config::SyncMode::CloudFirst),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg, &unused_mem_path())
            .await
            .expect("mode line");
        assert!(line.contains("cloud_first"), "got: {line}");
        // item 38: cloud_first has no local write queue to report on.
        assert!(!line.contains("pending"), "got: {line}");
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn mode_line_explicit_offline_shown_even_without_server_url() {
        clear_no_server_env();
        // An explicit mode is sync configuration worth surfacing on its own.
        let cfg = crate::config::Config {
            mode: Some(crate::config::SyncMode::Offline),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg, &unused_mem_path())
            .await
            .expect("explicit mode renders a line");
        assert!(line.contains("offline"), "got: {line}");
        // item 37: offline has no sync configuration to poll.
        assert!(!line.contains("pending"), "got: {line}");
    }

    // ── embed_threads_line: surfacing the override at the moment it matters ──

    #[test]
    fn single_threaded_server_names_the_override_variable() {
        let line = embed_threads_line(Some(1)).expect("a single-threaded budget is worth saying");
        assert!(
            line.contains("INKENTRY_EMBED_THREADS"),
            "the override is the whole point of the line: {line}"
        );
    }

    #[test]
    fn a_multi_threaded_or_unreported_budget_says_nothing() {
        for threads in [Some(2), Some(4), Some(64), None] {
            assert_eq!(
                embed_threads_line(threads),
                None,
                "only a single-threaded budget earns a line; got one for {threads:?}"
            );
        }
    }

    // ── embedding_state_line: what status knows about its own worker (no guessing) ──

    /// Numbers mirroring the recorded field repro: 42% of chunks searchable
    /// while only 21% of the token-weighted work is done.
    fn skewed_line(worker_alive: bool, embedder_unavailable: bool) -> Option<String> {
        embedding_state_line(
            worker_alive,
            embedder_unavailable,
            27_734,
            11_813,
            10_000_000,
            7_900_000,
            None,
        )
    }

    #[test]
    fn live_worker_reports_in_progress_with_both_labelled_measures() {
        let line = skewed_line(true, false).expect("pending work renders a line");
        assert!(line.contains("Embedding in progress"));
        assert!(
            line.contains("searchable 11813/27734 chunks (42%)"),
            "coverage stays chunk-shaped and labelled: {line}"
        );
        assert!(
            line.contains("21% of work done"),
            "progress is token-weighted and labelled: {line}"
        );
        assert!(
            !line.contains("may be running"),
            "the hedging parenthetical is deleted, not reworded: {line}"
        );
        assert!(
            !line.contains("resume"),
            "a live worker needs no resume advice: {line}"
        );
    }

    #[test]
    fn no_worker_with_pending_work_reports_incomplete_and_the_resume_command() {
        let line = skewed_line(false, false).expect("pending work renders a line");
        assert!(
            line.contains("Embedding incomplete"),
            "a dead worker is not 'in progress': {line}"
        );
        assert!(!line.contains("Embedding in progress"));
        assert!(
            line.contains("inkentry index ."),
            "must name the resume command: {line}"
        );
        assert!(!line.contains("may be running"));
    }

    #[test]
    fn unavailable_embedder_points_at_server_logs_instead_of_resume() {
        let line = skewed_line(false, true).expect("pending work renders a line");
        assert!(line.contains("Embedding incomplete"));
        assert!(line.contains("unavailable"), "must say so: {line}");
        assert!(
            line.contains("inkentry server logs"),
            "must point at the server logs: {line}"
        );
        assert!(
            !line.contains("resume with"),
            "resuming cannot help while the embedder is unavailable: {line}"
        );
    }

    #[test]
    fn coverage_and_progress_percentages_diverge_and_are_never_bare() {
        // The two measures answer different questions and must be rendered
        // under their own names; the field repro diverges 2x.
        let line = skewed_line(true, false).unwrap();
        assert!(line.contains("(42%)") && line.contains("21%"));
        assert!(line.contains("searchable") && line.contains("of work done"));
    }

    #[test]
    fn live_worker_line_carries_the_measured_eta_when_available() {
        let line = embedding_state_line(
            true,
            false,
            27_734,
            11_813,
            10_000_000,
            7_900_000,
            Some(std::time::Duration::from_secs(54 * 60)),
        )
        .unwrap();
        assert!(line.contains("~54 min left"), "got: {line}");
    }

    #[test]
    fn pre_backfill_index_omits_the_work_clause_instead_of_fabricating_it() {
        // total_tokens == 0: no denominator to weight work by, so the clause
        // is omitted (with the backfill hint), never rendered as a fake 0/100%.
        let line = embedding_state_line(false, false, 100, 40, 0, 0, None).unwrap();
        assert!(line.contains("searchable 40/100 chunks (40%)"));
        assert!(!line.contains("% of work done"));
        assert!(line.contains("--recount"), "hint at the backfill: {line}");
    }

    #[test]
    fn embedding_state_hidden_when_fully_embedded() {
        assert!(embedding_state_line(true, false, 100, 100, 10, 0, None).is_none());
        // Defensive: never render a negative pending count.
        assert!(embedding_state_line(true, false, 100, 120, 10, 0, None).is_none());
    }

    #[test]
    fn embedding_state_hidden_for_empty_index() {
        assert!(embedding_state_line(false, false, 0, 0, 0, 0, None).is_none());
    }

    // ── humanize_eta ─────────────────────────────────────────────────────────

    #[test]
    fn humanize_eta_scales_units() {
        use std::time::Duration;
        assert_eq!(humanize_eta(Duration::from_secs(30)), "~30s left");
        assert_eq!(humanize_eta(Duration::from_secs(54 * 60)), "~54 min left");
        assert_eq!(humanize_eta(Duration::from_secs(3_300)), "~55 min left");
        assert_eq!(humanize_eta(Duration::from_secs(6_000)), "~1h40m left");
    }

    // ── memory_backend_label: resolved-backend memory line (ADR-067 D3) ─────────

    #[test]
    fn memory_backend_label_maps_resolved_kinds() {
        // The label reflects the resolved backend_kind(), never the tier. Default
        // resolved backend is sqlite, so an offline repo must not read git-notes.
        assert_eq!(memory_backend_label("sqlite"), "sqlite (local)");
        assert_eq!(memory_backend_label("git-notes"), "git-notes (local)");
        assert_eq!(memory_backend_label("remote"), "remote (server)");
    }

    #[test]
    fn memory_backend_label_passes_through_unknown() {
        assert_eq!(memory_backend_label("future-kind"), "future-kind");
    }
}
