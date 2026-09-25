use anyhow::{Context as _, Result};
use clap::Args;

use inkentry_core::config::{self, org_tokens, server_keys};

#[derive(Args, Debug)]
pub struct LogoutArgs {
    /// Log out of a single organization, leaving every other cached session
    /// intact. Accepts a WorkOS org id or a slug (ADR-074 D4).
    #[arg(long)]
    pub org: Option<String>,
}

pub async fn logout(args: LogoutArgs) -> Result<()> {
    let store = config::default_secret_store()?;

    if let Some(target) = args.org.as_deref() {
        if org_tokens::clear_org(store.as_ref(), target)? {
            println!("Logged out of organization '{target}'.");
        } else {
            println!("No cached session for organization '{target}'.");
        }
        return Ok(());
    }

    let cleared = org_tokens::clear_all(store.as_ref())?;
    // Also strips a plaintext [auth] table so no session survives a bare logout.
    config::remove_auth_tokens()
        .context("removing a legacy [auth] table from ~/.config/inkentry/config.toml")?;
    match cleared {
        0 => println!("Logged out. No inkentry cloud sessions were cached."),
        1 => println!("Logged out. Cleared 1 cached inkentry cloud session."),
        n => println!("Logged out. Cleared {n} cached inkentry cloud sessions."),
    }

    // Server keys are deliberately left alone: someone recovering from a broken
    // cloud login must not lose keys used on other projects. This notice points
    // at the explicit command instead.
    let n = server_keys::count(store.as_ref())?;
    if n > 0 {
        println!(
            "{n} server key(s) are still stored (unaffected by this logout). \
             Run `inkentry auth remove-key --all-servers` to remove them all, or \
             `inkentry auth remove-key --server <url>` for just one."
        );
    }

    Ok(())
}
