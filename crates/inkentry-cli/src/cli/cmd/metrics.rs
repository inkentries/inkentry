//! `inkentry metrics` — ADR-098's deterministic state-metrics snapshot.
//!
//! CLI-thin: everything that reads `memory.db` or `git` lives in
//! `inkentry_core::metrics`; this module resolves the project, calls it, and
//! renders the result as JSON or a short human summary.

use anyhow::Result;
use clap::{Args, Subcommand};
use inkentry_core::metrics::{MedianSeconds, Rate, Snapshot, StatusMetricsSummary, build_snapshot};

use crate::config::Config;
use crate::storage::MemoryStore;

#[derive(Args, Debug)]
pub struct MetricsArgs {
    #[command(subcommand)]
    pub command: MetricsCommand,
}

#[derive(Subcommand, Debug)]
pub enum MetricsCommand {
    /// Compute a deterministic state-metrics snapshot for this project (ADR-098)
    Snapshot(MetricsSnapshotArgs),
}

#[derive(Args, Debug)]
pub struct MetricsSnapshotArgs {
    /// Metrics window, in days, ending at HEAD's commit time (or the newest
    /// entry's created_at outside a git repository)
    #[arg(long, default_value_t = inkentry_core::metrics::DEFAULT_WINDOW_DAYS)]
    pub window_days: u32,

    /// Print the snapshot as JSON (schema "inkentry.metrics/1") instead of a
    /// human summary
    #[arg(long)]
    pub json: bool,
}

pub async fn metrics(args: MetricsArgs, cfg: Config) -> Result<()> {
    match args.command {
        MetricsCommand::Snapshot(a) => snapshot(a, cfg).await,
    }
}

async fn snapshot(args: MetricsSnapshotArgs, cfg: Config) -> Result<()> {
    // ADR-067: fail closed when there is no local `.inkentry/` project rather
    // than reporting the machine-global store as if it were this project's.
    let db_path = crate::config::require_project_db(&cfg.db_path, false)?;
    let mem_path = db_path.with_file_name("memory.db");
    // `db_path` is always `<project_root>/.inkentry/index.db` (stability.md);
    // two `.parent()` calls recover the actual project root, which both
    // `derive_project_id`/`resolve_project_id` and the git commands need —
    // not `.inkentry/` itself, whose canonical path a non-git-repo fallback
    // id would hash instead of the project's own.
    let project_root = db_path
        .parent()
        .and_then(|p| p.parent())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let store = MemoryStore::open(&mem_path)?;
    let project_id = cfg.resolve_project_id(&project_root);
    let snap = build_snapshot(
        &store,
        &project_root,
        project_id,
        env!("CARGO_PKG_VERSION").to_string(),
        args.window_days,
    )
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&snap)?);
    } else {
        print_snapshot_summary(&snap);
    }
    Ok(())
}

fn print_snapshot_summary(snap: &Snapshot) {
    println!("inkentry metrics snapshot  (schema {})", snap.schema);
    println!("project      {}", snap.header.project);
    match &snap.header.commit {
        Some(c) => println!(
            "commit       {}  ({})",
            short_sha(&c.sha),
            format_timestamp(c.time)
        ),
        None => println!("commit       (not a git repository)"),
    }
    println!("window       {} days", snap.header.window_days);
    println!(
        "embedder     {}  dim={}  {}",
        snap.header.embedder.model_id, snap.header.embedder.dim, snap.header.embedder.precision
    );
    println!();

    let s = &snap.state;
    println!("rec.entries (in window, by kind)");
    for (kind, count) in &s.rec_entries.in_window {
        if *count > 0 {
            println!("  {kind:<12} {count}");
        }
    }
    match &s.rec_commit_coverage {
        Some(r) => println!("rec.commit_coverage         {}", format_pct(r)),
        None => println!("rec.commit_coverage         (not a git repository)"),
    }
    println!(
        "rec.supersede_rate           {}",
        format_pct(&s.rec_supersede_rate)
    );
    println!(
        "rec.time_to_supersede_p50    {}",
        format_median_duration(&s.rec_time_to_supersede_p50)
    );
    println!(
        "rec.open_question_age_p50    {}",
        format_median_duration(&s.rec_open_question_age_p50)
    );
    println!(
        "rec.near_duplicate_rate      {}  [{} excluded: no vector]",
        format_pct_value(
            s.rec_near_duplicate_rate.numerator,
            s.rec_near_duplicate_rate.denominator,
            s.rec_near_duplicate_rate.value
        ),
        s.rec_near_duplicate_rate.excluded_without_vector
    );
    println!(
        "rec.unresolved_conflicts     {}",
        s.rec_unresolved_conflicts
    );
    match &s.cmp_lines_per_decision {
        Some(r) => println!("cmp.lines_per_decision       {}", format_avg(r)),
        None => println!("cmp.lines_per_decision       (not a git repository)"),
    }
    println!(
        "cmp.review_items_per_day     {}",
        format_avg(&s.cmp_review_items_per_day)
    );
    println!(
        "cmp.tokens_context_estimate  {}",
        s.cmp_tokens_context_estimate
    );
}

/// The compact status section: the cheap subset of the metrics above,
/// printed as a few lines under `inkentry status`.
pub(super) fn print_status_summary(summary: &StatusMetricsSummary) {
    println!("Metrics ({}d window)", summary.window_days);
    let entries: Vec<String> = summary
        .rec_entries_in_window
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(kind, count)| format!("{kind}:{count}"))
        .collect();
    println!(
        "  entries      {}",
        if entries.is_empty() {
            "none".to_string()
        } else {
            entries.join(" ")
        }
    );
    match &summary.rec_commit_coverage {
        Some(r) => println!("  commits      {} covered", format_pct(r)),
        None => println!("  commits      (not a git repository)"),
    }
    println!(
        "  superseded   {}  (median time to supersede: {})",
        summary.rec_supersede_rate.numerator,
        format_bare_duration(summary.rec_time_to_supersede_p50.median_seconds)
    );
    println!(
        "  review/day   {}",
        summary
            .cmp_review_items_per_day
            .value
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "  open qs      {}  (median age: {})",
        summary.rec_open_question_age_p50.sample_size,
        format_bare_duration(summary.rec_open_question_age_p50.median_seconds)
    );
    println!("  conflicts    {}", summary.rec_unresolved_conflicts);
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn format_timestamp(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn format_pct(r: &Rate) -> String {
    format_pct_value(r.numerator, r.denominator, r.value)
}

fn format_pct_value(numerator: u64, denominator: u64, value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{numerator}/{denominator}  ({:.0}%)", v * 100.0),
        None => format!("{numerator}/{denominator}"),
    }
}

fn format_avg(r: &Rate) -> String {
    match r.value {
        Some(v) => format!("{}/{}  ({v:.2})", r.numerator, r.denominator),
        None => format!("{}/{}", r.numerator, r.denominator),
    }
}

fn format_median_duration(m: &MedianSeconds) -> String {
    match m.median_seconds {
        Some(secs) => format!("{}  (n={})", format_duration(secs), m.sample_size),
        None => format!("n/a  (n={})", m.sample_size),
    }
}

/// Like [`format_median_duration`] but without the trailing `(n=...)` —
/// for a caller (the `status` compact section) that already shows the
/// sample size as its own number and would otherwise double-parenthesise it.
fn format_bare_duration(median_seconds: Option<i64>) -> String {
    match median_seconds {
        Some(secs) => format_duration(secs),
        None => "n/a".to_string(),
    }
}

fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 3600 {
        format!("{}m", (secs / 60).max(1))
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_scales_units() {
        assert_eq!(format_duration(30), "1m");
        assert_eq!(format_duration(3_600), "1h");
        assert_eq!(format_duration(90_000), "1d");
    }

    #[test]
    fn format_pct_value_is_none_on_a_null_value_never_a_fabricated_percentage() {
        assert_eq!(format_pct_value(0, 0, None), "0/0");
    }

    #[test]
    fn format_pct_value_renders_a_rounded_percentage() {
        assert_eq!(format_pct_value(3, 4, Some(0.75)), "3/4  (75%)");
    }
}
