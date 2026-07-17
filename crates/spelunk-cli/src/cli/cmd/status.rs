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
    storage::{Database, open_memory_backend},
};

/// Stable JSON schema for `spelunk status --format json` (issue #269).
///
/// All fields listed here are guaranteed additive-safe: new optional fields
/// may be added in future versions, but existing fields will not be renamed or
/// removed. Consumers must tolerate unknown fields.
///
/// Field notes:
///
/// - `version` — spelunk CLI semver string (e.g. `"0.7.0"`)
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
/// Additional fields (`tier`, `mode`, `server_url`, `capabilities`,
/// `embedder_state`, `embedding_count`, `embedding_pending`,
/// `embed_worker_alive`, `embed_tokens`, `drift_candidates`, `usage_7d`) are
/// present for backward compatibility and richer tooling; treat them as
/// unstable extensions.
/// `embedder_state` mirrors the server's `/v1/health` readiness
/// (`"loading"`/`"ready"`/`"unavailable"`/`"disabled"`); it is `null` when
/// offline or when the reachable server pre-dates the readiness field.
/// `embedding_pending` is the chunk count still awaiting an embedding;
/// `embed_worker_alive` and `embed_tokens` describe the recorded embed
/// worker's liveness and token-weighted progress and are `null` when no embed
/// work is pending.
pub async fn status(args: StatusArgs, cfg: Config) -> Result<()> {
    let fmt = crate::utils::effective_format(&args.format);

    // JSON mode: current project stats only
    if fmt == "json" {
        // ADR-067: fail closed when there is no local `.spelunk/` project rather
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
        let pending_chunks = stats.chunk_count - stats.embedding_count;
        let (embed_worker_alive_json, embed_tokens_json) = if pending_chunks > 0 {
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
                "server_url": tier_url,
                "capabilities": caps_json,
                "embedder_state": embedder_state_json,
                "embedding_count": stats.embedding_count,
                "embedding_pending": pending_chunks,
                "embed_worker_alive": embed_worker_alive_json,
                "embed_tokens": embed_tokens_json,
                "drift_candidates": drift,
                "usage_7d": {
                    "search": usage_map.get("search").copied().unwrap_or(0),
                    "explore": usage_map.get("explore").copied().unwrap_or(0),
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
            println!("No projects registered. Run `spelunk index <path>` to get started.");
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
                println!("\x1b[1m{}\x1b[0m", p.root_path.display());
                if !p.root_path.exists() {
                    println!("  \x1b[31m[root path missing from disk]\x1b[0m");
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
                    Err(_) => println!("  \x1b[2m(no index yet)\x1b[0m"),
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
    // ADR-067: fail closed when there is no local `.spelunk/` project rather than
    // describing the global store. The scoped path also wins over a stray global
    // `index.db`.
    let db_path = match crate::config::require_project_db(&cfg.db_path, false) {
        Ok(p) => p,
        Err(_) => {
            println!("No spelunk project here. Run `spelunk init` first.");
            return Ok(());
        }
    };
    let tier = capability::get_tier(&cfg).await;

    let resolved = resolve_project_context(None, &cfg.db_path)?;

    if !db_path.exists() {
        println!("No index found for the current directory (checked parents too).");
        println!("Run `spelunk index <path>` to create one.");
        return Ok(());
    }

    let db = Database::open(&db_path)?;
    let s = db.stats()?;

    // ── Memory backend (single truthful line from the resolved backend, ADR-067 D3) ──
    let mem_path_text = db_path.with_file_name("memory.db");
    let mem_label = match open_memory_backend(&cfg, &mem_path_text, None).await {
        Ok(b) => memory_backend_label(b.backend_kind()).to_string(),
        Err(_) => "unavailable".to_string(),
    };

    // ── Capability tier section ───────────────────────────────────────────────
    print_tier_section(tier, &cfg, &mem_label);

    if let Some(p) = &resolved.project {
        println!("Project: \x1b[1m{}\x1b[0m", p.root_path.display());
    }
    println!("Index:      {}", db_path.display());
    println!("Files:      {}", s.file_count);
    println!("Chunks:     {}", s.chunk_count);
    println!("Embeddings: {}", s.embedding_count);
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
            println!("{line}");
        }
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
        println!("\n\x1b[33mDrift signals\x1b[0m  (unchanged while project evolved):");
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
        println!(
            "  \x1b[2mRun `spelunk search \"<topic>\"` to check if these are still relevant.\x1b[0m"
        );
    }

    // Usage summary (last 7 days)
    let usage = db.usage_last_7_days().unwrap_or_default();
    let total: i64 = usage.iter().map(|(_, n)| n).sum();
    if total > 0 {
        const COMMANDS: &[&str] = &["search", "explore", "memory search"];
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
fn print_tier_section(tier: &Tier, cfg: &Config, mem_label: &str) {
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
            println!("Capability tier:  \x1b[33mOffline\x1b[0m");
            if let Some(line) = sync_mode_line(cfg) {
                println!("{line}");
            }
            println!("  search          ast-grep + text{server_hint}");
            println!("  memory          {mem_label}");
            println!(
                "  explore         unavailable{}",
                explore_offline_hint(cfg.server_url.is_some())
            );
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
            println!("Capability tier:  \x1b[32mServer\x1b[0m  \x1b[2m({url_label})\x1b[0m");
            if let Some(line) = sync_mode_line(cfg) {
                println!("{line}");
            }
            let search_label = if caps.search_semantic {
                "ast-grep + text + semantic"
            } else {
                "ast-grep + text"
            };
            println!("  search          {search_label}");
            // Embedder readiness: explain *why* semantic search isn't in the
            // search line yet when the server is up but the model isn't ready.
            // Log hints must point at the probed server: `spelunk server logs`
            // reads the local daemon's logs, which are the wrong place when the
            // failing embedder lives on an explicit remote server_url.
            let remote_url = (!*auto_discovered).then_some(url.as_str());
            if let Some(line) = embedder_status_line(embedder_state, remote_url) {
                println!("{line}");
            }
            println!("  memory          {mem_label}");
            let explore_label = if caps.explore {
                "available"
            } else {
                "unavailable"
            };
            println!("  explore         {explore_label}");
        }
    }
    println!();
}

/// The `mode` line for `spelunk status`: a neutral one-word sync-mode
/// indicator. `None` on the solo default (no `server_url`, no explicit mode):
/// there is no sync configuration to surface. No call to action: the background
/// reconciler owns convergence, so status must not pre-teach a manual `spelunk
/// sync` workflow.
fn sync_mode_line(cfg: &Config) -> Option<String> {
    if cfg.server_url.is_none() && cfg.mode.is_none() {
        return None;
    }
    Some(format!("  {:<16}{}", "mode", cfg.resolve_mode().as_str()))
}

/// Hint for the `explore` line when the tier is Offline. With a configured
/// `server_url` the fix is never "set server_url" (it already is); the truthful
/// hint is that the configured server could not be reached.
fn explore_offline_hint(server_url_configured: bool) -> &'static str {
    if server_url_configured {
        "  [configured server unreachable]"
    } else {
        "  [set server_url to enable]"
    }
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

/// Render the `embedder` line for `spelunk status` (text mode) from the
/// server-side readiness state, or `None` when there is nothing useful to show
/// (an older server that never reported readiness). Pure so it can be unit
/// tested without capturing stdout.
///
/// `remote_url` is `Some` when the probed server came from an explicit
/// `server_url` (not loopback auto-discovery). The failure-hint must then point
/// at that server's own logs: `spelunk server logs` only reads the local
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
                 see `spelunk server logs`]"
                .to_string(),
        },
        EmbedderState::Ready => "  embedder        ready".to_string(),
        EmbedderState::Disabled => {
            "  embedder        disabled  [external embedding backend]".to_string()
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

/// Render the embedding-state line for `spelunk status` when the index has
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
        None => "work remaining unknown (run `spelunk index --recount` to backfill token counts)"
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
             the embedder is unavailable, see `spelunk server logs`"
        )
    } else {
        format!(
            "  \x1b[33mEmbedding incomplete\x1b[0m   {searchable}  \u{00b7}  {progress}; \
             resume with `spelunk index .`"
        )
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

    // ── embedder_status_line: `spelunk status` rendering of each state ──────────

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
        // `spelunk server logs` is the right place to look.
        let line = embedder_status_line(&EmbedderState::Unavailable, None)
            .expect("unavailable renders a line");
        assert!(line.contains("unavailable"));
        assert!(line.contains("failed to load"));
        assert!(line.contains("spelunk server logs"));
    }

    #[test]
    fn embedder_line_unavailable_remote_points_at_that_server_never_local_logs() {
        // Explicit server_url: `spelunk server logs` reads the LOCAL daemon's
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
            !line.contains("spelunk server logs"),
            "must not point a remote failure at local logs: {line}"
        );
    }

    #[test]
    fn embedder_line_ready_is_plain() {
        let line = embedder_status_line(&EmbedderState::Ready, None).expect("ready renders a line");
        assert!(line.contains("ready"));
    }

    #[test]
    fn embedder_line_disabled_notes_external_backend() {
        let line =
            embedder_status_line(&EmbedderState::Disabled, None).expect("disabled renders a line");
        assert!(line.contains("disabled"));
        assert!(line.contains("external"));
    }

    #[test]
    fn embedder_line_unknown_renders_nothing() {
        // Older server without the readiness field: no line rather than a
        // confusing "unknown".
        assert!(embedder_status_line(&EmbedderState::Unknown, None).is_none());
        assert!(embedder_status_line(&EmbedderState::Unknown, Some("https://t:1")).is_none());
    }

    // ── explore_offline_hint: truthful in both offline states ───────────────────

    #[test]
    fn explore_hint_without_server_url_suggests_setting_it() {
        assert!(explore_offline_hint(false).contains("set server_url"));
    }

    #[test]
    fn explore_hint_with_server_url_says_unreachable_not_set_it() {
        // server_url is already set; telling the operator to set it implies the
        // config is missing and hides the real problem (server unreachable).
        let hint = explore_offline_hint(true);
        assert!(hint.contains("unreachable"), "got: {hint}");
        assert!(!hint.contains("set server_url"), "got: {hint}");
    }

    // ── sync_mode_line: "local by design" vs "local because broken" ─────────────

    fn clear_no_server_env() {
        // SAFETY: serialised via #[serial] on every test that calls this, so no
        // other test reads/writes this env var concurrently.
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn mode_line_absent_on_solo_default() {
        clear_no_server_env();
        // No server_url, no explicit mode: nothing to explain, output unchanged.
        let cfg = crate::config::Config::default();
        assert!(sync_mode_line(&cfg).is_none());
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn mode_line_local_first_is_neutral_mode_word_without_call_to_action() {
        clear_no_server_env();
        let cfg = crate::config::Config {
            server_url: Some("https://team.example:7777".to_string()),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg).expect("server_url set renders a mode line");
        assert!(line.contains("local_first"), "got: {line}");
        // Neutral indicator only: no manual-sync imperative (the background
        // reconciler owns convergence).
        assert!(!line.contains("spelunk sync"), "got: {line}");
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn mode_line_cloud_first_is_neutral_mode_word() {
        clear_no_server_env();
        let cfg = crate::config::Config {
            server_url: Some("https://team.example:7777".to_string()),
            mode: Some(crate::config::SyncMode::CloudFirst),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg).expect("mode line");
        assert!(line.contains("cloud_first"), "got: {line}");
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn mode_line_explicit_offline_shown_even_without_server_url() {
        clear_no_server_env();
        // An explicit mode is sync configuration worth surfacing on its own.
        let cfg = crate::config::Config {
            mode: Some(crate::config::SyncMode::Offline),
            ..Default::default()
        };
        let line = sync_mode_line(&cfg).expect("explicit mode renders a line");
        assert!(line.contains("offline"), "got: {line}");
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
            line.contains("spelunk index ."),
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
            line.contains("spelunk server logs"),
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
