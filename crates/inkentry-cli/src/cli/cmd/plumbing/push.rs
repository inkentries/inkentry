use std::io::Write;

use anyhow::{Context, Result};
use serde::Serialize;

use super::PlumbingPushArgs;
use crate::{
    capability,
    cli::cmd::auth_api,
    cli::cmd::memory::sync::{LocalEmbedPolicy, push_local_oneway},
    config::Config,
    storage::{CloudSyncClient, MemoryStore},
};

// Field names and types are the stability contract. `interrupted` is always
// false: an interrupted run exits 2 without a report.
#[derive(Serialize)]
struct PushReport {
    attempted: usize,
    created: u32,
    skipped: u32,
    failed: u32,
    already_synced: usize,
    edges_pushed: usize,
    without_local_vector: usize,
    embedded_locally: usize,
    interrupted: bool,
}

pub(super) async fn push(
    args: PlumbingPushArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("plumbing push", tier, cfg.server_url.as_deref())?;
    let base_url = capability::require_explicit_server_url("plumbing push", cfg)?;
    let project_id = cfg.project_id.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "`project_id` is not configured. Set it in `.inkentry/config.toml` \
             or via `INKENTRY_PROJECT_ID`."
        )
    })?;

    let local = MemoryStore::open(mem_path)
        .with_context(|| format!("opening local memory at {}", mem_path.display()))?;
    let key = auth_api::ensure_fresh_server_key(cfg, &base_url).await?;
    let client = CloudSyncClient::new(
        &base_url,
        &project_id,
        key.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )?;

    let accepts_pushed_vectors = tier.caps().is_some_and(|c| c.accepts_pushed_vectors);
    let local_embed = LocalEmbedPolicy::resolve(cfg, mem_path);
    let summary = push_local_oneway(
        &local,
        &client,
        args.include_archived,
        accepts_pushed_vectors,
        args.force,
        &local_embed,
    )
    .await?;

    // Exit 2 with empty stdout: an interruption or total failure must be
    // distinguishable from an empty delta, so neither is a report.
    if let Some(reason) = summary.interrupted.as_deref() {
        anyhow::bail!(
            "pushed {} of {} entries, then stopped: {reason}. \
             Re-run to resume (already-pushed entries are skipped).",
            summary.created + summary.skipped,
            summary.attempted
        );
    }
    if summary.created == 0 && summary.skipped == 0 && summary.failed > 0 {
        anyhow::bail!(
            "push failed: 0 of {} entries reached the server ({} failed).",
            summary.attempted,
            summary.failed
        );
    }

    let report = PushReport {
        attempted: summary.attempted,
        created: summary.created,
        skipped: summary.skipped,
        failed: summary.failed,
        already_synced: summary.already_synced,
        edges_pushed: summary.edges_pushed,
        without_local_vector: summary.without_local_vector,
        embedded_locally: summary.embedded_locally,
        interrupted: false,
    };
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{}", serde_json::to_string(&report)?)?;
    stdout.flush()?;

    // Exit 1 is an empty delta; the report is emitted either way.
    if summary.created == 0 {
        std::process::exit(1);
    }
    Ok(())
}
