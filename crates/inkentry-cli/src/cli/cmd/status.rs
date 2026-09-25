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

pub async fn status(args: StatusArgs, cfg: Config) -> Result<()> {
    let fmt = crate::utils::effective_format(&args.format);

    if fmt == "json" {
        // Fail closed without a local `.inkentry/` project rather than reporting the global store.
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
        let mem_path = db_path.with_file_name("memory.db");
        let usage = events_command_counts_last_7_days(&mem_path);
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

        // Best-effort: a computation failure omits the section rather than failing `status`.
        let metrics_json = metrics_summary_json(&mem_path, &db_path).await;

        // Poll-and-apply before reading the pending count: a poll can change what
        // is outstanding, so reading first would pair a stale count with a fresh
        // `last_synced_at`.
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

        // Silent failure: full-text still returns these entries, so recall looks complete.
        let memory_embedding_pending: Option<usize> = MemoryStore::open(&mem_path)
            .ok()
            .and_then(|s| s.notes_missing_embeddings(false).ok())
            .map(|v| v.len());

        let has_semantic_search = matches!(
            tier,
            Tier::Server { caps, .. } if caps.search_semantic
        );

        let last_indexed_at: Option<String> = stats.last_indexed.and_then(|ts| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        });

        let embedding_dim: Option<u64> = if stats.embedding_count > 0 {
            Some(crate::embeddings::EMBEDDING_DIM as u64)
        } else {
            None
        };

        let (tier_str, tier_url, caps_json) = match tier {
            Tier::Offline(_) => ("offline", serde_json::Value::Null, serde_json::Value::Null),
            Tier::Server { url, caps, .. } => (
                "server",
                serde_json::Value::String(url.clone()),
                serde_json::to_value(caps).unwrap_or(serde_json::Value::Null),
            ),
        };

        let embedder_state_json: serde_json::Value = match tier.embedder_state() {
            Some(capability::EmbedderState::Unknown) | None => serde_json::Value::Null,
            Some(s) => serde_json::Value::String(s.as_str().to_string()),
        };

        // Worker liveness must consider `refresh_pending` as well as coverage: a
        // re-embed drain can be live while coverage reads 100%.
        let pending_chunks = stats.pending_embed_count();
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
        let embedding_refresh_pending_json = if refresh_pending > 0 {
            serde_json::json!(refresh_pending)
        } else {
            serde_json::Value::Null
        };
        let summary_scheme_json = match db.summary_scheme() {
            Ok(Some(s)) => serde_json::Value::String(s),
            _ => serde_json::Value::Null,
        };

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
                // Stable fields: never renamed or removed, only added to.
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
                // Extensions: may change.
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
                "text_only_count": stats.text_only_count,
                "embedding_refresh_pending": embedding_refresh_pending_json,
                "summary_scheme": summary_scheme_json,
                "index_rebuilt_from": rebuilt_unpopulated,
                "embed_worker_alive": embed_worker_alive_json,
                "embed_tokens": embed_tokens_json,
                "drift_candidates": drift,
                "usage_7d": {
                    "search": usage_map.get("search").copied().unwrap_or(0),
                    "memory_search": usage_map.get("memory search").copied().unwrap_or(0),
                },
                "metrics": metrics_json,
            }))?
        );
        return Ok(());
    }

    let show_all = args.all || args.list;

    if show_all {
        let reg = Registry::open().context("opening registry")?;
        let projects = reg.all_projects()?;

        if projects.is_empty() {
            println!("No projects registered. Run `inkentry index <path>` to get started.");
            return Ok(());
        }

        if args.list {
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

    // Fail closed without a local `.inkentry/` project rather than describing the global store.
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

    let mem_path_text = db_path.with_file_name("memory.db");
    let mem_label = match open_memory_backend(&cfg, &mem_path_text, None).await {
        Ok(b) => memory_backend_label(b.backend_kind()).to_string(),
        Err(_) => "unavailable".to_string(),
    };

    print_tier_section(tier, &cfg, &mem_label, &mem_path_text).await;

    if let Some(p) = &resolved.project {
        cprintln!("Project: \x1b[1m{}\x1b[0m", p.root_path.display());
    }
    println!("Index:      {}", db_path.display());
    println!("Files:      {}", s.file_count);
    println!("Chunks:     {}", s.chunk_count);
    if s.text_only_count > 0 {
        println!(
            "Embeddings: {} ({} chunks full-text only)",
            s.embedding_count, s.text_only_count
        );
    } else {
        println!("Embeddings: {}", s.embedding_count);
    }
    if let Some(line) = rebuilt_line(db.unpopulated_since_rebuild().unwrap_or(None)) {
        cprintln!("{line}");
    }
    if s.pending_embed_count() > 0 {
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
            s.embeddable_count(),
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

    let usage = events_command_counts_last_7_days(&mem_path_text);
    let total: i64 = usage.iter().map(|(_, n)| n).sum();
    if total > 0 {
        const COMMANDS: &[&str] = &["search", "context"];
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

    // Best-effort: a computation failure (no git, an unreadable store) skips the section.
    if mem_path_text.exists()
        && let Some(summary) = metrics_status_summary(&mem_path_text, &db_path).await
    {
        println!();
        super::metrics::print_status_summary(&summary);
    }

    Ok(())
}

fn events_command_counts_last_7_days(mem_path: &std::path::Path) -> Vec<(String, i64)> {
    if !mem_path.exists() {
        return Vec::new();
    }
    const SEVEN_DAYS_SECS: i64 = 7 * 24 * 3600;
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        - SEVEN_DAYS_SECS;
    MemoryStore::open(mem_path)
        .ok()
        .and_then(|s| s.events_command_counts_since(cutoff).ok())
        .unwrap_or_default()
}

async fn metrics_status_summary(
    mem_path: &std::path::Path,
    db_path: &std::path::Path,
) -> Option<inkentry_core::metrics::StatusMetricsSummary> {
    let store = MemoryStore::open(mem_path).ok()?;
    // `db_path` is `<project_root>/.inkentry/index.db`.
    let project_root = db_path.parent().and_then(|p| p.parent())?;
    inkentry_core::metrics::build_status_summary(
        &store,
        project_root,
        inkentry_core::metrics::DEFAULT_WINDOW_DAYS,
    )
    .await
    .ok()
}

async fn metrics_summary_json(
    mem_path: &std::path::Path,
    db_path: &std::path::Path,
) -> serde_json::Value {
    if !mem_path.exists() {
        return serde_json::Value::Null;
    }
    match metrics_status_summary(mem_path, db_path).await {
        Some(summary) => serde_json::to_value(summary).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

async fn print_tier_section(
    tier: &Tier,
    cfg: &Config,
    mem_label: &str,
    mem_path: &std::path::Path,
) {
    match tier {
        Tier::Offline(reason) => {
            // Keyed to why the probe gave up, not to `server_url`: under the
            // kill-switch no URL is read, so a config-derived hint would
            // recommend an action that cannot help.
            let server_hint =
                capability::offline_search_hint(*reason, capability::explicit_probe_failure());
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
            let remote_url = (!*auto_discovered).then_some(url.as_str());
            if let Some(line) = embedder_status_line(embedder_state, remote_url) {
                cprintln!("{line}");
            }
            println!("  memory          {mem_label}");
        }
    }
    println!();
}

// No call to action: the background reconciler owns convergence, so status must
// not pre-teach a manual `inkentry sync` workflow.
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

// Poll before reading `pending`: a poll can apply acks, so reading first would
// pair a stale count with a fresh "last synced". A fresh project stays silent
// rather than printing a hollow "up to date".
async fn sync_status_suffix(cfg: &Config, mem_path: &std::path::Path) -> Option<String> {
    let store = MemoryStore::open(mem_path).ok()?;
    let poll = crate::cli::cmd::memory::outbox::poll_and_apply(cfg, mem_path).await;
    let pending = store.pending_sync_count().ok()?;
    let last_synced_at = poll.as_ref().and_then(|p| p.last_synced_at);
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

fn memory_backend_label(kind: &str) -> &str {
    match kind {
        "sqlite" => "sqlite (local)",
        "git-notes" => "git-notes (local)",
        "remote" => "remote (server)",
        other => other,
    }
}

// `remote_url` is set for an explicit `server_url`; the failure hint must then
// name that server, since `inkentry server logs` reads only the local daemon's log.
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
        // Pre-readiness server: say nothing rather than "unknown".
        EmbedderState::Unknown => return None,
    };
    Some(line)
}

fn labelled_pct(done: i64, total: i64) -> Option<u64> {
    (done.max(0) as u64)
        .saturating_mul(100)
        .checked_div(u64::try_from(total).ok().filter(|t| *t > 0)?)
}

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

// Hybrid search still returns these via full-text, so recall degrades silently.
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

// An emptied index prints the same zeros as a never-indexed project.
fn rebuilt_line(rebuilt_from: Option<i32>) -> Option<String> {
    let found = rebuilt_from?;
    Some(format!(
        "\x1b[33m[Index: emptied by a rebuild from {}, not yet reindexed; \
         run 'inkentry index .']\x1b[0m",
        super::helpers::replaced_schema(found)
    ))
}

// `searchable` is chunk coverage and `of work done` is token-weighted progress;
// they diverge by design, so each is labelled.
fn embedding_state_line(
    worker_alive: bool,
    embedder_unavailable: bool,
    embeddable_count: i64,
    embedding_count: i64,
    total_tokens: i64,
    pending_tokens: i64,
    eta: Option<std::time::Duration>,
) -> Option<String> {
    if embeddable_count <= 0 || embedding_count >= embeddable_count {
        return None;
    }
    let coverage = labelled_pct(embedding_count, embeddable_count).unwrap_or(0);
    let searchable =
        format!("searchable {embedding_count}/{embeddable_count} chunks ({coverage}%)");

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

// Only a budget of 1 makes a first index take hours, and the override is otherwise
// discoverable only in the server log.
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

    #[test]
    fn embedder_line_loading_advises_warmup() {
        let line =
            embedder_status_line(&EmbedderState::Loading, None).expect("loading renders a line");
        assert!(line.contains("loading"));
        assert!(line.contains("warming up"));
    }

    #[test]
    fn embedder_line_unavailable_loopback_points_at_local_logs() {
        let line = embedder_status_line(&EmbedderState::Unavailable, None)
            .expect("unavailable renders a line");
        assert!(line.contains("unavailable"));
        assert!(line.contains("failed to load"));
        assert!(line.contains("inkentry server logs"));
    }

    #[test]
    fn embedder_line_unavailable_remote_points_at_that_server_never_local_logs() {
        let line = embedder_status_line(
            &EmbedderState::Unavailable,
            Some("https://team.example:4655"),
        )
        .expect("unavailable renders a line");
        assert!(line.contains("unavailable"));
        assert!(line.contains("https://team.example:4655"), "got: {line}");
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
        assert!(embedder_status_line(&EmbedderState::Unknown, None).is_none());
        assert!(embedder_status_line(&EmbedderState::Unknown, Some("https://t:1")).is_none());
    }

    fn clear_no_server_env() {
        // SAFETY: serialised via #[serial] on every test that calls this, so no
        // other test reads/writes this env var concurrently.
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
    }

    fn unused_mem_path() -> std::path::PathBuf {
        std::path::PathBuf::from(":memory:")
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn mode_line_absent_on_solo_default() {
        clear_no_server_env();
        let cfg = crate::config::Config::default();
        assert!(sync_mode_line(&cfg, &unused_mem_path()).await.is_none());
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn mode_line_local_first_is_neutral_mode_word_without_call_to_action() {
        clear_no_server_env();
        // Isolate from a real local daemon: local_first polls the relay via `INKENTRY_STATE_DIR`.
        let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
        let tmp_state = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var("INKENTRY_STATE_DIR", tmp_state.path()) };

        let cfg = crate::config::Config {
            server_url: Some("https://team.example:4655".to_string()),
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
        assert!(!line.contains("inkentry sync"), "got: {line}");
        assert!(!line.contains("pending"), "got: {line}");
    }

    // The relay only connects to declared team targets; declare the mock server.
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
            server_url: Some("https://team.example:4655".to_string()),
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
        assert!(!line.contains("inkentry sync"), "got: {line}");
        assert!(!line.contains("last synced"), "got: {line}");
    }

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

        let db_dir = tempfile::TempDir::new().unwrap();
        let db =
            inkentry_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
                .unwrap();
        let instance_id = db.get_or_create_instance_id().unwrap();
        let relay_instance_id = instance_id.clone();
        let state = inkentry_server::AppState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
            auth: std::sync::Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
            conflict_threshold: inkentry_server::default_conflict_threshold(),
            embedder: inkentry_server::EmbedderSlot::disabled(),
            embed_admission: inkentry_server::EmbedAdmission::new(
                inkentry_server::EMBED_QUEUE_CAPACITY,
                inkentry_server::EMBED_INTERACTIVE_CAPACITY_HIGH,
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
        let prev_trust = std::env::var_os("INKENTRY_TEST_TRUST_RECORDED_RESPONDER");
        let tmp_state = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("INKENTRY_STATE_DIR", tmp_state.path());
            // The relay is in-process, so the recorded pid is this test binary and
            // the OS query cannot match; the seam relaxes only that (the instance
            // id is still checked).
            std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", "1");
        }
        std::fs::write(
            tmp_state.path().join("server.port"),
            format!("{relay_port}\n"),
        )
        .unwrap();
        std::fs::write(
            tmp_state.path().join("server.pid"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        std::fs::write(
            tmp_state.path().join("server.instance_id"),
            format!("{relay_instance_id}\n"),
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

        // The relay's detached push task needs a moment.
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
            match prev_trust {
                Some(v) => std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", v),
                None => std::env::remove_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER"),
            }
        }

        let line = line.expect("status line must show 'last synced' after the relay syncs");
        assert!(line.contains("local_first"), "got: {line}");
        assert!(line.contains("last synced"), "got: {line}");
        assert!(!line.contains("inkentry sync"), "got: {line}");
    }

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

        let db_dir = tempfile::TempDir::new().unwrap();
        let db =
            inkentry_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
                .unwrap();
        let instance_id = db.get_or_create_instance_id().unwrap();
        let relay_instance_id = instance_id.clone();
        let state = inkentry_server::AppState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
            auth: std::sync::Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
            conflict_threshold: inkentry_server::default_conflict_threshold(),
            embedder: inkentry_server::EmbedderSlot::disabled(),
            embed_admission: inkentry_server::EmbedAdmission::new(
                inkentry_server::EMBED_QUEUE_CAPACITY,
                inkentry_server::EMBED_INTERACTIVE_CAPACITY_HIGH,
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
        let prev_trust = std::env::var_os("INKENTRY_TEST_TRUST_RECORDED_RESPONDER");
        let tmp_state = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("INKENTRY_STATE_DIR", tmp_state.path());
            // The relay is in-process, so the recorded pid is this test binary and
            // the OS query cannot match; the seam relaxes only that (the instance
            // id is still checked).
            std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", "1");
        }
        std::fs::write(
            tmp_state.path().join("server.port"),
            format!("{relay_port}\n"),
        )
        .unwrap();
        std::fs::write(
            tmp_state.path().join("server.pid"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        std::fs::write(
            tmp_state.path().join("server.instance_id"),
            format!("{relay_instance_id}\n"),
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
        // Mounted with the note's real id so the ack lands on the row (`poll_and_apply` does a `has_note` lookup).
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

        // Poll `sync_mode_line` directly so the first call to see "last synced" is the
        // one whose own poll applied the ack.
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
            match prev_trust {
                Some(v) => std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", v),
                None => std::env::remove_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER"),
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
            server_url: Some("https://team.example:4655".to_string()),
            mode: Some(crate::config::SyncMode::CloudFirst),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg, &unused_mem_path())
            .await
            .expect("mode line");
        assert!(line.contains("cloud_first"), "got: {line}");
        assert!(!line.contains("pending"), "got: {line}");
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn mode_line_explicit_offline_shown_even_without_server_url() {
        clear_no_server_env();
        let cfg = crate::config::Config {
            mode: Some(crate::config::SyncMode::Offline),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg, &unused_mem_path())
            .await
            .expect("explicit mode renders a line");
        assert!(line.contains("offline"), "got: {line}");
        assert!(!line.contains("pending"), "got: {line}");
    }

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

    // Numbers from a field repro: 42% of chunks searchable, 21% of work done.
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
        let line = embedding_state_line(false, false, 100, 40, 0, 0, None).unwrap();
        assert!(line.contains("searchable 40/100 chunks (40%)"));
        assert!(!line.contains("% of work done"));
        assert!(line.contains("--recount"), "hint at the backfill: {line}");
    }

    #[test]
    fn embedding_state_hidden_when_fully_embedded() {
        assert!(embedding_state_line(true, false, 100, 100, 10, 0, None).is_none());
        // Never render a negative pending count.
        assert!(embedding_state_line(true, false, 100, 120, 10, 0, None).is_none());
    }

    #[test]
    fn embedding_state_hidden_for_empty_index() {
        assert!(embedding_state_line(false, false, 0, 0, 0, 0, None).is_none());
    }

    #[test]
    fn humanize_eta_scales_units() {
        use std::time::Duration;
        assert_eq!(humanize_eta(Duration::from_secs(30)), "~30s left");
        assert_eq!(humanize_eta(Duration::from_secs(54 * 60)), "~54 min left");
        assert_eq!(humanize_eta(Duration::from_secs(3_300)), "~55 min left");
        assert_eq!(humanize_eta(Duration::from_secs(6_000)), "~1h40m left");
    }

    #[test]
    fn memory_backend_label_maps_resolved_kinds() {
        assert_eq!(memory_backend_label("sqlite"), "sqlite (local)");
        assert_eq!(memory_backend_label("git-notes"), "git-notes (local)");
        assert_eq!(memory_backend_label("remote"), "remote (server)");
    }

    #[test]
    fn memory_backend_label_passes_through_unknown() {
        assert_eq!(memory_backend_label("future-kind"), "future-kind");
    }
}
