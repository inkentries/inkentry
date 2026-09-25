use std::io::Write;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    capability,
    cli::cmd::auth_api,
    cli::cmd::memory::sync::{LocalEmbedPolicy, pull_and_apply},
    config::Config,
    storage::{CloudSyncClient, MemoryStore},
};

// A non-zero `without_local_vector` tells a scripted caller that entries landed
// text-only rather than searchable.
#[derive(Serialize)]
struct PullReport {
    applied: usize,
    embedded_locally: usize,
    without_local_vector: usize,
}

pub(super) async fn pull(mem_path: &std::path::Path, cfg: &Config) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("plumbing pull", tier, cfg.server_url.as_deref())?;
    let base_url = capability::require_explicit_server_url("plumbing pull", cfg)?;
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

    // Failures reach exit 2 via main; an empty page (including a 404) is an
    // empty delta, not an error.
    let local_embed = LocalEmbedPolicy::resolve(cfg, mem_path);
    let summary = pull_and_apply(&local, &client, &local_embed).await?;

    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&PullReport {
            applied: summary.applied,
            embedded_locally: summary.embedded_locally,
            without_local_vector: summary.without_local_vector,
        })?
    )?;
    stdout.flush()?;

    // Exit 1 is an empty delta; the report is emitted either way.
    if summary.applied == 0 {
        std::process::exit(1);
    }
    Ok(())
}
