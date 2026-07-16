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

/// The stderr label for a read served from the local store while a team
/// `server_url` is configured (ADR-037: the default `local_first` mode reads
/// locally and converges the server replica via `spelunk sync`).
///
/// `None` when no `server_url` is set (solo path stays silent), when reads
/// route to the server (`cloud_first`), when the user explicitly opted out of
/// server contact (`offline`), or when git notes is the read source.
pub(crate) fn local_read_notice(
    cfg: &Config,
    backend_override: Option<&str>,
) -> Option<&'static str> {
    use crate::config::SyncMode;
    if backend_override == Some("git-notes")
        || cfg.server_url.is_none()
        || cfg.resolve_mode() != SyncMode::LocalFirst
    {
        return None;
    }
    Some(
        "note: showing local data (mode \"local_first\"); the team server was not \
         consulted for these entries. Run \"spelunk sync\" to converge, or set \
         mode = \"cloud_first\" in .spelunk/config.toml.",
    )
}

/// Open the memory backend for a READ command, labeling on stderr when local
/// data is served despite a configured team `server_url`. stderr only:
/// stdout stays machine-clean for `--format json`/`jsonl`.
pub(crate) async fn open_read_backend(
    cfg: &Config,
    mem_path: &std::path::Path,
    backend_override: Option<&str>,
) -> Result<Box<dyn crate::storage::MemoryBackend + Send>> {
    if let Some(notice) = local_read_notice(cfg, backend_override) {
        eprintln!("{notice}");
    }
    crate::storage::open_memory_backend(cfg, mem_path, backend_override).await
}

/// Build a `ServerInferenceClient` from config, returning an error if
/// `server_url` is not configured.
pub(crate) fn require_server_client(cfg: &Config, feature: &str) -> Result<ServerInferenceClient> {
    // Inference-only feature: a local `spelunk server start` is enough, so the
    // guidance must not tell a solo user to configure a team `server_url`.
    // `cfg.server_url` here is the effective config, so it is `None` for an
    // auto-discovered loopback and `Some` only for an explicit team URL.
    ServerInferenceClient::from_config(cfg).ok_or_else(|| {
        anyhow::anyhow!(crate::capability::inference_server_required_message(
            feature
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

/// `O_NOFOLLOW`, which `std` does not expose. Defined here to avoid pulling in
/// the `libc` crate for a single constant. `0` on platforms without the flag.
#[cfg(unix)]
pub(crate) fn libc_o_nofollow() -> i32 {
    #[cfg(target_os = "macos")]
    {
        0x0000_0100
    }
    #[cfg(target_os = "linux")]
    {
        0o400_000
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

/// Open `path` `0600` for writing, truncating, refusing to follow a symlink at
/// `path`.
///
/// These files live at fixed, predictable locations; on a shared host an
/// attacker could pre-create a symlink there pointing at an arbitrary file the
/// spelunk user can write, turning a routine open into an overwrite primitive.
/// `O_NOFOLLOW` (Unix) makes the open fail instead of following such a link.
pub(crate) fn open_private_file_for_write(path: &std::path::Path) -> Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc_o_nofollow())
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }
}

#[cfg(test)]
mod local_read_notice_tests {
    use super::local_read_notice;
    use crate::config::{Config, SyncMode};

    fn clear_no_server_env() {
        // SAFETY: serialised via #[serial] on every test in this module, so no
        // other test reads/writes this env var concurrently.
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn no_server_url_is_silent() {
        clear_no_server_env();
        let cfg = Config::default();
        assert_eq!(local_read_notice(&cfg, None), None);
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn server_url_default_mode_labels_local_read() {
        clear_no_server_env();
        let cfg = Config {
            server_url: Some("https://team.example.com:7777".to_string()),
            ..Default::default()
        };
        let notice = local_read_notice(&cfg, None).expect("local_first read must be labeled");
        assert!(notice.contains("local_first"), "got: {notice}");
        assert!(notice.contains("spelunk sync"), "got: {notice}");
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn cloud_first_is_silent_reads_route_remote() {
        clear_no_server_env();
        let cfg = Config {
            server_url: Some("https://team.example.com:7777".to_string()),
            mode: Some(SyncMode::CloudFirst),
            ..Default::default()
        };
        assert_eq!(local_read_notice(&cfg, None), None);
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn explicit_offline_is_silent() {
        clear_no_server_env();
        let cfg = Config {
            server_url: Some("https://team.example.com:7777".to_string()),
            mode: Some(SyncMode::Offline),
            ..Default::default()
        };
        assert_eq!(local_read_notice(&cfg, None), None);
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn git_notes_override_is_silent() {
        clear_no_server_env();
        let cfg = Config {
            server_url: Some("https://team.example.com:7777".to_string()),
            ..Default::default()
        };
        assert_eq!(local_read_notice(&cfg, Some("git-notes")), None);
    }

    #[test]
    #[serial_test::serial(spelunk_no_server_env)]
    fn no_server_kill_switch_silences_notice_despite_server_url_and_default_mode() {
        // SPELUNK_NO_SERVER=1 forces resolve_mode() to Offline (Config::resolve_mode,
        // highest precedence) even though `mode` is unset and `server_url` is set,
        // which would otherwise default to local_first and label the read.
        clear_no_server_env();
        let cfg = Config {
            server_url: Some("https://team.example.com:7777".to_string()),
            ..Default::default()
        };
        // SAFETY: serialised via #[serial(spelunk_no_server_env)]; cleared above
        // and below so no other test in this group observes it set.
        unsafe { std::env::set_var("SPELUNK_NO_SERVER", "1") };
        assert_eq!(
            local_read_notice(&cfg, None),
            None,
            "the offline kill-switch must win over the default local_first notice"
        );
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }
}
