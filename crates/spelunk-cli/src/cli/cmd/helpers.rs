use anyhow::{Context, Result};

use crate::{
    config::{Config, require_project_db},
    embeddings::vec_to_blob,
    server_client::ServerInferenceClient,
    storage::Database,
};

/// ADR-067: resolve the project's `index.db` fail-closed via
/// [`require_project_db`] (no machine-global fallback), error if it does not
/// exist, then open it. An explicit `--db` bypasses the project gate. In an
/// un-`init`'d dir this refuses with the ADR-067 message instead of reading the
/// global `~/.config/spelunk/index.db`.
pub(crate) fn open_project_db(
    db: Option<&std::path::Path>,
    cfg_path: &std::path::Path,
) -> Result<(std::path::PathBuf, Database)> {
    let db_path = match db {
        Some(p) => p.to_path_buf(),
        None => require_project_db(cfg_path, false)?,
    };
    if !db_path.exists() {
        anyhow::bail!(
            "No index found (checked current directory and parents).\n\
             Run `spelunk index <path>` inside your project first."
        );
    }
    let database = Database::open(&db_path)?;
    Ok((db_path, database))
}

/// Build a `ServerInferenceClient` from config, returning an error if
/// `server_url` is not configured.
pub(crate) fn require_server_client(cfg: &Config, feature: &str) -> Result<ServerInferenceClient> {
    // Inference-only feature: a local `spelunk server start` is enough, so the
    // guidance must not tell a solo user to configure a team `server_url`
    // (oss^133). `cfg.server_url` here is the effective config, so it is `None`
    // for an auto-discovered loopback and `Some` only for an explicit team URL.
    ServerInferenceClient::from_config(cfg).ok_or_else(|| {
        anyhow::anyhow!(crate::capability::inference_server_required_message(
            feature,
            cfg.server_url.as_deref()
        ))
    })
}

/// Embed a query with the given F2LLM instruction and return the raw float vector.
///
/// `task` is the full instruction string (e.g. "Given a question, retrieve …").
/// The format matches F2LLM-v2-330M's expected query prompt:
/// `Instruct: <task>\nQuery: <query>`.
pub(crate) async fn embed_query_vec(
    client: &ServerInferenceClient,
    task: &str,
    query: &str,
) -> Result<Vec<f32>> {
    let query_text = format!("Instruct: {task}\nQuery: {query}");
    client.embed_text(&query_text).await
}

/// Embed a query with the given task prefix and return the blob bytes suitable
/// for KNN search.
pub(crate) async fn embed_query(
    client: &ServerInferenceClient,
    task: &str,
    query: &str,
) -> Result<Vec<u8>> {
    let vec = embed_query_vec(client, task, query).await?;
    Ok(vec_to_blob(&vec))
}

/// Return the final path component of `path` as a display name, falling back
/// to the full path string if there is no file name component.
pub(crate) fn project_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Detach: re-exec this binary with the same CLI arguments but without
/// `--detach`, with all stdio closed, so the caller (e.g. a git hook) regains
/// its prompt immediately while spelunk continues in the background.
pub(crate) fn spawn_detached() -> Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach")
        .collect();
    std::process::Command::new(exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning detached background process")?;
    Ok(())
}
