//! `inkentry plumbing pull` — one-way server→local memory delta pull with a
//! JSONL report.
//!
//! Plumbing counterpart of the porcelain `inkentry memory pull`: the same core
//! (`pull_and_apply`, cursored on `MAX(remote_id)`) and the same egress
//! guards, emitting one machine-readable report object and following the
//! plumbing 0/1/2 exit contract. It makes an outbound request, so it requires
//! an explicitly-configured team `server_url`.

use std::io::Write;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    capability,
    cli::cmd::auth_api,
    cli::cmd::memory::sync::pull_and_apply,
    config::Config,
    storage::{CloudSyncClient, MemoryStore},
};

/// The one report object emitted on a completed run (exit 0 or 1). `applied` is
/// the number of new remote entries written to the local store this run.
#[derive(Serialize)]
struct PullReport {
    applied: usize,
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

    // A network/auth/setup failure returns Err and reaches exit 2 via main; an
    // empty page (nothing new, including a 404) returns Ok(0) and is an empty
    // delta, not an error.
    let applied = pull_and_apply(&local, &client).await?;

    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "{}",
        serde_json::to_string(&PullReport { applied })?
    )?;
    stdout.flush()?;

    // Exit 1 is an empty delta (nothing new to apply); exit 0 means at least
    // one entry was applied. The report is emitted in both cases.
    if applied == 0 {
        std::process::exit(1);
    }
    Ok(())
}
