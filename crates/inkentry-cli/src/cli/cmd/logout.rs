//! `inkentry logout`: clear stored inkentry cloud credentials.
//!
//! Bare `logout` clears every cached WorkOS session (ADR-074 D4) and strips any
//! legacy plaintext `[auth]` remnant left by an older client. `logout --org
//! <target>` clears just that one organization's session, leaving every other
//! cached org intact — symmetric with ADR-071's per-server key removal.
//!
//! It does not touch self-hosted server keys as a side effect (ADR-071 D3,
//! founder-review correction): a developer recovering from a broken cloud login
//! should not silently lose the server key(s) they use on other projects.
//! Removing those is the explicit `inkentry auth remove-key`.
//!
//! The residual-key notice below is the bridge between the two: someone who
//! reaches for `logout` looking for key removal, as issue #120's reporter did,
//! is told here that server keys exist, how many, and which command removes one.

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
    // Strip any legacy plaintext [auth] remnant too, so a bare logout leaves
    // neither a cached session nor a plaintext one behind (ADR-074).
    config::remove_auth_tokens()
        .context("removing a legacy [auth] table from ~/.config/inkentry/config.toml")?;
    match cleared {
        0 => println!("Logged out. No inkentry cloud sessions were cached."),
        1 => println!("Logged out. Cleared 1 cached inkentry cloud session."),
        n => println!("Logged out. Cleared {n} cached inkentry cloud sessions."),
    }

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
