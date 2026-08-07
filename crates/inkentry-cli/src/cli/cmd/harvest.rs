//! `inkentry harvest`: capture memory from git history and session logs.
//!
//! Harvest is two things wearing one command: the one-time backfill over a
//! range of history, and the continuous capture the post-commit hook runs after
//! every commit. It shares its whole implementation, and its memory-store
//! resolution, with the deprecated `inkentry memory harvest` alias.

use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

use super::memory::MemoryHarvestArgs;
use crate::config::Config;

/// Arguments for the top-level `inkentry harvest` command: the harvest options,
/// plus the memory-store overrides that `memory harvest` reaches through the
/// `memory` command's globals.
#[derive(Args, Debug)]
pub struct HarvestArgs {
    #[command(flatten)]
    pub harvest: MemoryHarvestArgs,

    /// Path to the memory database (overrides auto-detect)
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Storage backend: sqlite (default) or git-notes
    #[arg(long, default_value = "sqlite", value_name = "BACKEND")]
    pub backend: String,
}

/// Top-level `inkentry harvest`. Delegates to the shared harvest runner, which
/// resolves the memory store identically to `inkentry memory harvest`.
pub async fn harvest(args: HarvestArgs, cfg: Config) -> Result<()> {
    super::memory::run_harvest(args.harvest, args.db, &args.backend, &cfg).await
}
