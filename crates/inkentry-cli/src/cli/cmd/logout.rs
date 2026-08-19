//! `inkentry logout`: clear stored inkentry cloud credentials.
//!
//! It clears **only** the `[auth]` WorkOS token pair written by
//! `inkentry login`: the credential logout exists to undo. It does not touch
//! self-hosted server keys as a side effect (ADR-071 D3, founder-review
//! correction): a developer recovering from a broken cloud login should not
//! silently lose the server key(s) they use on other projects. Removing those
//! is an explicit, separate action, and since ADR-090 it is spelled
//! `inkentry auth remove-key`, next to the `set-key` that installed them.
//!
//! The residual-key notice below is the bridge between the two: someone who
//! reaches for `logout` looking for key removal, as issue #120's reporter did,
//! is told here that server keys exist, how many, and which command removes
//! one.

use anyhow::{Context as _, Result};
use clap::Args;

use inkentry_core::config::{self, server_keys};

#[derive(Args, Debug)]
pub struct LogoutArgs {}

pub async fn logout(_args: LogoutArgs) -> Result<()> {
    config::remove_auth_tokens()
        .context("removing [auth] tokens from ~/.config/inkentry/config.toml")?;
    println!("Logged out. Stored inkentry cloud credentials have been removed.");

    let store = config::default_secret_store()?;
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
