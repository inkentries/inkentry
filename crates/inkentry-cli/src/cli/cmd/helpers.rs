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
/// global `~/.config/inkentry/index.db`.
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
             Run `inkentry index <path>` inside your project first."
        );
    }
    let database = Database::open(&db_path)?;
    announce_index_rebuild(&database);
    Ok((db_path, database))
}

/// How to name the schema a rebuild discarded. Version `0` is an index from
/// before `user_version` was stamped at all, so there is no number to print.
pub(crate) fn replaced_schema(found: i32) -> String {
    if found == 0 {
        "an older, unstamped schema".to_string()
    } else {
        format!("schema version {found}")
    }
}

/// State the rebuild on the run that performed it.
///
/// [`Database::rebuilt_from`] is `None` on every other open, so a normal run
/// prints nothing. Stderr, not the log: the rebuild's `tracing::warn!` sits
/// below the CLI's default `error` filter, and raising that filter would
/// surface every unrelated warning in the workspace with it.
pub(crate) fn announce_index_rebuild(db: &Database) {
    let Some(found) = db.rebuilt_from() else {
        return;
    };
    crate::notice::enotice!(
        "notice: this index was written by {} and cannot be read by this build, so it was \
         rebuilt empty (recorded usage history was kept). Run `inkentry index .` to \
         repopulate it.",
        replaced_schema(found)
    );
}

/// Build a `ServerInferenceClient` from config, returning an error if
/// `server_url` is not configured.
pub(crate) fn require_server_client(cfg: &Config, feature: &str) -> Result<ServerInferenceClient> {
    // Inference-only feature: a local `inkentry server start` is enough, so the
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

/// Where a detached run's diagnostics land, relative to the project's
/// `.inkentry/` directory. Named in `hooks install` output, so a user told that
/// something runs on every commit is also told where it reports.
pub(crate) const BACKGROUND_LOG_NAME: &str = ".inkentry/background.log";

/// The absolute path of [`BACKGROUND_LOG_NAME`] for the project the current
/// directory sits in, or `None` outside a project.
pub(crate) fn background_log_path() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    inkentry_core::config::find_project_dir(&cwd).map(|d| d.join("background.log"))
}

/// Open a log for appending, `0600`, refusing to follow a symlink at the path.
///
/// Appending rather than truncating is what lets the two detached runs a
/// single commit fires (index, then harvest) both survive in one file, and it
/// keeps a fresh spawn from punching a hole through the output of a detached
/// child that is still writing to the same log at its own offset.
pub(crate) fn open_log_for_append(path: &std::path::Path) -> Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc_o_nofollow());
    }
    opts.open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// Detach: re-exec this binary with the same CLI arguments but without
/// `--detach`, so the caller (e.g. a git hook) regains its prompt immediately
/// while inkentry continues in the background.
///
/// The child's output goes to the project's background log rather than to a
/// null sink: detached from a terminal, a null sink turns every failure into
/// silence, and the failures worth reporting (an unavailable LLM stopping
/// `harvest`) recur on every commit without ever being seen. Inheriting the
/// parent's streams is not the alternative: a pipe reader (`git commit`, CI)
/// blocks until the detached child exits, and one that closes first SIGPIPEs
/// the child mid-run.
///
/// A log that cannot be opened falls back to the null sink, since diagnostics
/// must never be what stops the work from starting.
pub(crate) fn spawn_detached() -> Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach")
        .collect();

    let mut cmd = std::process::Command::new(exe);
    cmd.args(&args).stdin(std::process::Stdio::null());

    match background_log_path().and_then(|p| open_log_pair(&p, &args)) {
        Some((out, err)) => {
            cmd.stdout(out).stderr(err);
        }
        None => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
    }

    let _std_handles = StdHandlesNotInherited::for_spawn();
    cmd.spawn()
        .context("spawning detached background process")?;
    Ok(())
}

/// Two independent handles onto the append-opened log, with a header naming the
/// run already written: an error alone says nothing about which of a commit's
/// two background runs produced it, or when.
fn open_log_pair(
    path: &std::path::Path,
    args: &[String],
) -> Option<(std::fs::File, std::fs::File)> {
    use std::io::Write;
    let mut out = open_log_for_append(path).ok()?;
    let _ = writeln!(
        out,
        "\n=== inkentry {} ({}) ===",
        args.join(" "),
        chrono::Utc::now().to_rfc3339()
    );
    let err = out.try_clone().ok()?;
    Some((out, err))
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
/// inkentry user can write, turning a routine open into an overwrite primitive.
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

/// Keeps a detached child from inheriting the standard handles this process
/// holds but never hands it, for as long as the value lives.
///
/// Windows inherits by handle table, not by stream. `CreateProcessW` is called
/// with `bInheritHandles = TRUE`, which duplicates every handle marked
/// inheritable in this process, not only the three named in `STARTUPINFO`. So
/// a parent whose own stdout is a pipe passes that pipe to a detached child
/// that redirected all three of its own streams and never writes to it. Nothing
/// in the child ever closes it, so the reader sees no EOF until the child
/// exits: the whole embed pass for `index`, the daemon's lifetime for
/// `server start`. Redirecting the child's streams, which every spawn site here
/// already does, does not address it, because the leaked handle is not one of
/// the three the child was given.
///
/// Clearing `HANDLE_FLAG_INHERIT` for the span of the spawn stops the copy
/// being made while leaving this process free to keep writing to its own
/// stdout. The previous flags are restored on drop, so an ordinary child
/// spawned afterwards still inherits normally.
///
/// Scope: the standard handles only. Any other inheritable handle open in this
/// process is still copied, so if a pipe reader still blocks with this in
/// place, the culprit is a non-standard handle and the answer is
/// `STARTUPINFOEX` with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, which means
/// calling `CreateProcessW` directly instead of through `std::process`.
///
/// Not thread safe: the flag is a property of the process, not of the caller.
/// Every user of this is a CLI path spawning from the main thread.
///
/// Inert on Unix, where `Stdio::null()` genuinely closes the stream in the
/// child and every other descriptor is close-on-exec.
#[cfg(windows)]
pub(crate) struct StdHandlesNotInherited {
    restored_on_drop: Vec<*mut std::ffi::c_void>,
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    pub(super) const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    pub(super) const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    pub(super) const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    pub(super) const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

    unsafe extern "system" {
        pub(super) fn GetStdHandle(nStdHandle: u32) -> *mut c_void;
        pub(super) fn GetHandleInformation(hObject: *mut c_void, lpdwFlags: *mut u32) -> i32;
        pub(super) fn SetHandleInformation(hObject: *mut c_void, dwMask: u32, dwFlags: u32) -> i32;
    }
}

#[cfg(windows)]
impl StdHandlesNotInherited {
    /// Clear the inherit flag on every standard handle that currently carries
    /// it. Handles that are absent, unqueryable or already non-inheritable are
    /// skipped, so nothing here can fail the spawn it precedes.
    pub(crate) fn for_spawn() -> Self {
        let mut restored_on_drop = Vec::new();
        for id in [
            win::STD_INPUT_HANDLE,
            win::STD_OUTPUT_HANDLE,
            win::STD_ERROR_HANDLE,
        ] {
            // SAFETY: each call takes a handle this process owns, or a null or
            // invalid handle that the guards below reject before use.
            let handle = unsafe { win::GetStdHandle(id) };
            if handle.is_null() || handle as isize == -1 {
                continue;
            }
            let mut flags = 0u32;
            if unsafe { win::GetHandleInformation(handle, &mut flags) } == 0
                || flags & win::HANDLE_FLAG_INHERIT == 0
            {
                continue;
            }
            if unsafe { win::SetHandleInformation(handle, win::HANDLE_FLAG_INHERIT, 0) } != 0 {
                restored_on_drop.push(handle);
            }
        }
        Self { restored_on_drop }
    }
}

#[cfg(windows)]
impl Drop for StdHandlesNotInherited {
    fn drop(&mut self) {
        for handle in self.restored_on_drop.drain(..) {
            // SAFETY: the handle was queried successfully above and this
            // process still owns it; restoring the flag it had on entry.
            unsafe {
                win::SetHandleInformation(
                    handle,
                    win::HANDLE_FLAG_INHERIT,
                    win::HANDLE_FLAG_INHERIT,
                )
            };
        }
    }
}

#[cfg(not(windows))]
pub(crate) struct StdHandlesNotInherited;

#[cfg(not(windows))]
impl StdHandlesNotInherited {
    pub(crate) fn for_spawn() -> Self {
        Self
    }
}
