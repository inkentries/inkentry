//! `inkentry auth set-key` / `remove-key` / `list-servers`: manage the
//! per-server bearer credentials a self-hosted `server_url` resolves through
//! (ADR-071 D1/D3), plus the credential for a configured LLM endpoint.
//!
//! These are the credential's front door: the key is read from stdin or an
//! interactive prompt, never from argv (a positional or flag-valued secret
//! lands in shell history and `ps` output). `set-key --server` stores it in
//! the per-origin map (`inkentry_core::config::server_keys`); `set-key --llm`
//! stores the single LLM credential (`inkentry_core::config::llm_key`);
//! `list-servers` prints only origins, never key material.
//!
//! `remove-key` is the same surface run backwards, mirroring `set-key` flag
//! for flag so the undo is derivable from the install without reading
//! anything (ADR-090). It reports whether it found a credential rather than
//! printing the same sentence for a revocation and a typo, and it normalizes
//! nothing itself: the origin a URL means is decided by the one function
//! `set-key` already stores under.

use anyhow::{Context, Result};
use clap::{ArgGroup, Args, Subcommand};
use std::io::{IsTerminal, Write};

use inkentry_core::config::{llm_key, server_keys};

#[derive(Args, Debug)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Store a credential, read from stdin/prompt: a self-hosted inkentry-server's
    /// bearer key (`--server`) or the LLM endpoint's key (`--llm`)
    SetKey(AuthSetKeyArgs),
    /// Remove a stored credential: one server's key (`--server`), the LLM
    /// endpoint's key (`--llm`), or every server key (`--all-servers`)
    RemoveKey(AuthRemoveKeyArgs),
    /// List servers with a stored key (origins only, never prints key material)
    ListServers,
}

#[derive(Args, Debug)]
#[command(group(
    ArgGroup::new("key_kind")
        .required(true)
        .multiple(false)
        .args(["server", "llm"])
))]
pub struct AuthSetKeyArgs {
    /// Server URL this key belongs to (normalized to its origin before storage)
    #[arg(long)]
    pub server: Option<String>,

    /// Store the credential for the configured LLM endpoint (`llm_url`) instead
    /// of a server key. The value is read from stdin/prompt, never argv.
    #[arg(long)]
    pub llm: bool,
}

#[derive(Args, Debug)]
#[command(group(
    ArgGroup::new("removal_kind")
        .required(true)
        .multiple(false)
        .args(["server", "llm", "all_servers"])
))]
pub struct AuthRemoveKeyArgs {
    /// Server URL whose key to remove (normalized to its origin, exactly as
    /// `set-key` stored it)
    #[arg(long)]
    pub server: Option<String>,

    /// Remove the stored credential for the configured LLM endpoint (`llm_url`)
    #[arg(long)]
    pub llm: bool,

    /// Remove every stored server key. Spelled for what it clears: the LLM
    /// credential is not a server key and is left alone.
    #[arg(long)]
    pub all_servers: bool,
}

pub async fn auth(args: AuthArgs) -> Result<()> {
    match args.command {
        AuthCommand::SetKey(a) => match a.server.as_deref() {
            Some(server) => set_server_key(server),
            // The ArgGroup guarantees exactly one of the two was supplied.
            None => set_llm_key(),
        },
        AuthCommand::RemoveKey(a) => match a.server.as_deref() {
            Some(server) => remove_server_key(server),
            // The ArgGroup guarantees exactly one of the three was supplied.
            None if a.llm => remove_llm_key(),
            None => remove_all_server_keys(),
        },
        AuthCommand::ListServers => list_servers(),
    }
}

fn set_server_key(server: &str) -> Result<()> {
    let key = read_secret_from_stdin_or_prompt("Server key")?;
    let store = inkentry_core::config::default_secret_store()?;
    let origin = server_keys::set_key_for_origin(server, &key, store.as_ref())
        .context("storing the server key")?;
    println!("Stored a server key for {origin}.");
    Ok(())
}

fn set_llm_key() -> Result<()> {
    let key = read_secret_from_stdin_or_prompt("LLM key")?;
    let store = inkentry_core::config::default_secret_store()?;
    llm_key::set_with_store(&key, store.as_ref()).context("storing the LLM key")?;
    println!("Stored an LLM key in the {} secret store.", store.kind());
    Ok(())
}

fn remove_server_key(server: &str) -> Result<()> {
    let store = inkentry_core::config::default_secret_store()?;
    let outcome = server_keys::clear_origin(server, store.as_ref())
        .context("removing the stored server key")?;
    if outcome.removed {
        println!("Removed the stored server key for {}.", outcome.origin);
    } else {
        println!("No server key was stored for {}.", outcome.origin);
    }
    Ok(())
}

fn remove_all_server_keys() -> Result<()> {
    let store = inkentry_core::config::default_secret_store()?;
    let removed = server_keys::clear_all(store.as_ref()).context("removing the server keys")?;
    if removed > 0 {
        println!("Removed {removed} stored server key(s).");
    } else {
        println!("No server keys were stored.");
    }
    Ok(())
}

fn remove_llm_key() -> Result<()> {
    let store = inkentry_core::config::default_secret_store()?;
    if llm_key::clear_with_store(store.as_ref()).context("removing the LLM key")? {
        println!("Removed the stored LLM key.");
    } else {
        println!("No LLM key was stored.");
    }
    Ok(())
}

fn list_servers() -> Result<()> {
    let store = inkentry_core::config::default_secret_store()?;
    let origins = server_keys::list_origins(store.as_ref())?;
    if origins.is_empty() {
        println!("No server keys stored.");
        return Ok(());
    }
    for origin in &origins {
        println!("{origin}");
    }
    Ok(())
}

/// Read a secret from stdin: piped input if present, else an interactive
/// prompt labelled `label`. Never accepted via a CLI flag/argv (D3).
fn read_secret_from_stdin_or_prompt(label: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        eprint!("{label}: ");
        std::io::stderr().flush().ok();
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .with_context(|| format!("reading {} from stdin", label.to_lowercase()))?;
    let key = line.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("no {} provided (empty input)", label.to_lowercase());
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkentry_core::config::secret_store::MemoryStore;

    #[test]
    fn set_key_then_list_servers_round_trip_via_store() {
        let store = MemoryStore::default();
        let origin =
            server_keys::set_key_for_origin("https://team.example:4655/ignored", "sk-1", &store)
                .unwrap();
        assert_eq!(origin, "https://team.example:4655");

        assert_eq!(
            server_keys::list_origins(&store).unwrap(),
            vec!["https://team.example:4655".to_string()]
        );
    }
}
