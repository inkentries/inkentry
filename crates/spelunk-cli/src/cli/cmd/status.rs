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

use super::search::resolve_project_and_deps;
use crate::{
    capability::{self, Tier},
    config::{Config, resolve_db},
    registry::Registry,
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
///
/// Additional fields (`tier`, `server_url`, `capabilities`, `snapshot_count`,
/// `drift_candidates`, `usage_7d`) are present for backward compatibility and
/// richer tooling; treat them as unstable extensions.
pub async fn status(args: StatusArgs, cfg: Config) -> Result<()> {
    let fmt = crate::utils::effective_format(&args.format);

    // JSON mode: current project stats only
    if fmt == "json" {
        let tier = capability::get_tier(&cfg).await;

        // Resolve project and DB path. Prefer the registry-registered path so
        // `project` can be populated; fall back to config default if needed.
        let reg = Registry::open().ok();
        let project_entry = reg.as_ref().and_then(|r| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| r.find_project_for_path(&cwd).ok().flatten())
        });
        let db_path = match &project_entry {
            Some(p) => p.db_path.clone(),
            None => {
                let (p, _) = resolve_project_and_deps(None, &cfg)?;
                p
            }
        };
        let project_root: Option<String> = project_entry
            .as_ref()
            .map(|p| p.root_path.display().to_string());

        let db = Database::open(&db_path)?;
        let stats = db.stats()?;
        let languages = db.language_stats().unwrap_or_default();
        let drift = db.drift_candidates(30, 10).unwrap_or_default();
        let usage = db.usage_last_7_days().unwrap_or_default();
        let mem_path = resolve_db(None, &cfg.db_path).with_file_name("memory.db");
        let memory_count = match open_memory_backend(&cfg, &mem_path, None).ok() {
            Some(b) => b.count().await.unwrap_or(0),
            None => 0,
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
            use std::time::{Duration, UNIX_EPOCH};
            UNIX_EPOCH
                .checked_add(Duration::from_secs(ts as u64))
                .map(|t| {
                    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                    // Format as RFC3339/ISO-8601 without pulling in chrono.
                    let s = secs;
                    let (date, time) = iso8601_from_unix(s);
                    format!("{date}T{time}Z")
                })
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
            Tier::Server { url, caps } => (
                "server",
                serde_json::Value::String(url.clone()),
                serde_json::to_value(caps).unwrap_or(serde_json::Value::Null),
            ),
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
                // ── Extensions (backward-compat, may change) ─────────────────
                "tier": tier_str,
                "server_url": tier_url,
                "capabilities": caps_json,
                "embedding_count": stats.embedding_count,
                "snapshot_count": stats.snapshot_count,
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
                "{:<6}  {:<8}  {:<10}  Root",
                "Files", "Chunks", "Embeddings"
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
                    "{:<6}  {:<8}  {:<10}  {}{}",
                    files,
                    chunks,
                    embeddings,
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

    // Current project only
    let tier = capability::get_tier(&cfg).await;

    let reg = Registry::open().ok();
    let project = reg.as_ref().and_then(|r| {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| r.find_project_for_path(&cwd).ok().flatten())
    });

    let db_path = match &project {
        Some(p) => p.db_path.clone(),
        None => resolve_db(None, &cfg.db_path),
    };

    if !db_path.exists() {
        println!("No index found for the current directory (checked parents too).");
        println!("Run `spelunk index <path>` to create one.");
        return Ok(());
    }

    let db = Database::open(&db_path)?;
    let s = db.stats()?;

    // ── Capability tier section ───────────────────────────────────────────────
    print_tier_section(tier, &cfg);

    if let Some(p) = &project {
        println!("Project: \x1b[1m{}\x1b[0m", p.root_path.display());
    }
    println!("Index:      {}", db_path.display());
    println!("Files:      {}", s.file_count);
    println!("Chunks:     {}", s.chunk_count);
    println!("Embeddings: {}", s.embedding_count);
    if s.snapshot_count > 0 {
        println!("Snapshots:  {}", s.snapshot_count);
    }
    if let Some(ts) = s.last_indexed {
        println!("Last index: {}", format_age(ts));
    }

    // Show dependencies
    if let (Some(reg), Some(p)) = (&reg, &project) {
        let deps = reg.get_deps(p.id)?;
        if !deps.is_empty() {
            println!("\nDependencies:");
            for dep in &deps {
                let dep_stats = Database::open(&dep.db_path).and_then(|db| db.stats()).ok();
                let summary = dep_stats
                    .map(|s| format!("{} files, {} chunks", s.file_count, s.chunk_count))
                    .unwrap_or_else(|| "not indexed".to_string());
                println!("  → {}  ({})", dep.root_path.display(), summary);
            }
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

fn print_tier_section(tier: &Tier, cfg: &Config) {
    match tier {
        Tier::Offline => {
            let server_hint = if cfg.server_url.is_some() {
                "  [unreachable]"
            } else {
                "  [set server_url to enable semantic search]"
            };
            println!("Capability tier:  \x1b[33mOffline\x1b[0m");
            println!("  search          ast-grep + text{server_hint}");
            println!("  memory          git-notes (local)");
            println!("  explore         unavailable  [set server_url to enable]");
            println!("  plan            unavailable  [set server_url to enable]");
        }
        Tier::Server { url, caps } => {
            println!("Capability tier:  \x1b[32mServer\x1b[0m  \x1b[2m({url})\x1b[0m");
            let search_label = if caps.search_semantic {
                "ast-grep + text + semantic"
            } else {
                "ast-grep + text"
            };
            println!("  search          {search_label}");
            let mem_label = if caps.memory_push {
                "git-notes + server sync"
            } else {
                "git-notes (local)"
            };
            println!("  memory          {mem_label}");
            let explore_label = if caps.explore {
                "available"
            } else {
                "unavailable"
            };
            println!("  explore         {explore_label}");
            let plan_label = if caps.plan {
                "available"
            } else {
                "unavailable"
            };
            println!("  plan            {plan_label}");
        }
    }
    println!();
}

/// Convert a Unix timestamp (seconds) to a `(date, time)` tuple formatted as
/// `"YYYY-MM-DD"` and `"HH:MM:SS"` (UTC), without pulling in an external date
/// library.
///
/// Returns `("1970-01-01", "00:00:00")` for `ts = 0`.
fn iso8601_from_unix(ts: u64) -> (String, String) {
    // Days from epoch
    let days = ts / 86400;
    let rem = ts % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;

    // Gregorian calendar computation (no external crate)
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    (
        format!("{:04}-{:02}-{:02}", y, mo, d),
        format!("{:02}:{:02}:{:02}", h, m, s),
    )
}

pub(crate) fn format_age(unix_ts: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    if let Ok(t) = UNIX_EPOCH
        .checked_add(Duration::from_secs(unix_ts as u64))
        .ok_or(())
        && let Ok(elapsed) = std::time::SystemTime::now().duration_since(t)
    {
        let secs = elapsed.as_secs();
        return if secs < 60 {
            format!("{secs}s ago")
        } else if secs < 3600 {
            format!("{}m ago", secs / 60)
        } else if secs < 86400 {
            format!("{}h ago", secs / 3600)
        } else {
            format!("{}d ago", secs / 86400)
        };
    }
    "unknown".to_string()
}
