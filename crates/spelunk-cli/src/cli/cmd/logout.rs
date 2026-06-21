//! `spelunk logout` — remove the stored `api_key` from the global config.

use anyhow::{Context as _, Result};

use spelunk_core::config;

pub async fn logout() -> Result<()> {
    config::remove_api_key().context("removing api_key from ~/.config/spelunk/config.toml")?;
    println!("Logged out. Your api_key has been removed from ~/.config/spelunk/config.toml");
    Ok(())
}
