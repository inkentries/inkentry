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
/// Additional fields (`tier`, `server_url`, `capabilities`, `embedder_state`,
/// `drift_candidates`, `usage_7d`) are present for backward compatibility and
/// richer tooling; treat them as unstable extensions.
/// `embedder_state` mirrors the server's `/v1/health` readiness
/// (`"loading"`/`"ready"`/`"unavailable"`/`"disabled"`); it is `null` when
/// offline or when the reachable server pre-dates the readiness field.
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

        // Server-side embedder readiness (spelunk-oss^50). `null` when offline
        // or when talking to a server that pre-dates the readiness field.
        let embedder_state_json: serde_json::Value = match tier.embedder_state() {
            Some(capability::EmbedderState::Unknown) | None => serde_json::Value::Null,
            Some(s) => serde_json::Value::String(s.as_str().to_string()),
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
                "server_url": tier_url,
                "capabilities": caps_json,
                "embedder_state": embedder_state_json,
                "embedding_count": stats.embedding_count,
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
    // Surface an in-progress (or interrupted) embed pass: when chunks outnumber
    // embeddings there is embedding work left, e.g. a detached `--detach-embed`
    // run still working through batches, or an interrupted run to resume. This
    // is the completion check for a backgrounded embed (spelunk-oss^74).
    if let Some(line) = embedding_progress_line(s.chunk_count, s.embedding_count) {
        println!("{line}");
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
                "  [unreachable]"
            } else {
                "  [set server_url to enable semantic search]"
            };
            println!("Capability tier:  \x1b[33mOffline\x1b[0m");
            println!("  search          ast-grep + text{server_hint}");
            println!("  memory          {mem_label}");
            println!("  explore         unavailable  [set server_url to enable]");
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
            let search_label = if caps.search_semantic {
                "ast-grep + text + semantic"
            } else {
                "ast-grep + text"
            };
            println!("  search          {search_label}");
            // Embedder readiness: explain *why* semantic search isn't in the
            // search line yet when the server is up but the model isn't ready.
            if let Some(line) = embedder_status_line(embedder_state) {
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
/// tested without capturing stdout (spelunk-oss^50).
fn embedder_status_line(state: &capability::EmbedderState) -> Option<String> {
    use capability::EmbedderState;
    let line = match state {
        EmbedderState::Loading => {
            "  embedder        \x1b[33mloading\x1b[0m  [model warming up — retry shortly]"
                .to_string()
        }
        EmbedderState::Unavailable => {
            "  embedder        \x1b[31munavailable\x1b[0m  [model failed to load — see `spelunk server logs`]"
                .to_string()
        }
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

/// Render the "embedding in progress" line for `spelunk status` when the index
/// has more chunks than embeddings, i.e. an embed pass is still running (e.g. a
/// detached `--detach-embed` subprocess) or was interrupted and can be resumed.
/// Returns `None` when every chunk is embedded (or the index is empty), so a
/// fully-embedded index prints nothing extra. Pure so it can be unit tested
/// (spelunk-oss^74).
fn embedding_progress_line(chunk_count: i64, embedding_count: i64) -> Option<String> {
    if chunk_count <= 0 || embedding_count >= chunk_count {
        return None;
    }
    let pending = chunk_count - embedding_count;
    Some(format!(
        "  \x1b[33mEmbedding in progress\x1b[0m  {embedding_count}/{chunk_count} embedded \
         ({pending} pending) \x1b[2m(a background embed may be running; re-run \
         `spelunk index` to resume if not)\x1b[0m"
    ))
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
        let line = embedder_status_line(&EmbedderState::Loading).expect("loading renders a line");
        assert!(line.contains("loading"));
        assert!(line.contains("warming up"));
    }

    #[test]
    fn embedder_line_unavailable_points_at_logs() {
        let line =
            embedder_status_line(&EmbedderState::Unavailable).expect("unavailable renders a line");
        assert!(line.contains("unavailable"));
        assert!(line.contains("failed to load"));
        assert!(line.contains("spelunk server logs"));
    }

    #[test]
    fn embedder_line_ready_is_plain() {
        let line = embedder_status_line(&EmbedderState::Ready).expect("ready renders a line");
        assert!(line.contains("ready"));
    }

    #[test]
    fn embedder_line_disabled_notes_external_backend() {
        let line = embedder_status_line(&EmbedderState::Disabled).expect("disabled renders a line");
        assert!(line.contains("disabled"));
        assert!(line.contains("external"));
    }

    #[test]
    fn embedder_line_unknown_renders_nothing() {
        // Older server without the readiness field: no line rather than a
        // confusing "unknown".
        assert!(embedder_status_line(&EmbedderState::Unknown).is_none());
    }

    // ── embedding_progress_line: detached / interrupted embed signal ────────────
    // (spelunk-oss^74)

    #[test]
    fn embedding_progress_shown_when_chunks_outnumber_embeddings() {
        let line = embedding_progress_line(100, 40).expect("partial embed shows a line");
        assert!(line.contains("Embedding in progress"));
        assert!(line.contains("40/100"));
        assert!(line.contains("60 pending"));
    }

    #[test]
    fn embedding_progress_hidden_when_fully_embedded() {
        assert!(embedding_progress_line(100, 100).is_none());
        // Defensive: never render a negative pending count.
        assert!(embedding_progress_line(100, 120).is_none());
    }

    #[test]
    fn embedding_progress_hidden_for_empty_index() {
        assert!(embedding_progress_line(0, 0).is_none());
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
