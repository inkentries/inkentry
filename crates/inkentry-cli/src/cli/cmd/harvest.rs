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

pub async fn harvest(args: HarvestArgs, cfg: Config) -> Result<()> {
    super::memory::run_harvest(args.harvest, args.db, &args.backend, &cfg).await
}
