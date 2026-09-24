use anyhow::{Context, Result};

use crate::{
    config::{Config, require_project_db},
    embeddings::vec_to_blob,
    server_client::ServerInferenceClient,
    storage::Database,
};

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

// Version 0 predates `user_version` stamping, so there is no number to print.
pub(crate) fn replaced_schema(found: i32) -> String {
    if found == 0 {
        "an older, unstamped schema".to_string()
    } else {
        format!("schema version {found}")
    }
}

// Stderr rather than the log: the rebuild's `tracing::warn!` is below the CLI's
// default `error` filter.
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

pub(crate) fn require_server_client(cfg: &Config, feature: &str) -> Result<ServerInferenceClient> {
    ServerInferenceClient::from_config(cfg).ok_or_else(|| {
        anyhow::anyhow!(crate::capability::inference_server_required_message(
            feature
        ))
    })
}

pub(crate) async fn embed_query_vec(
    client: &ServerInferenceClient,
    task: &str,
    query: &str,
) -> Result<Vec<f32>> {
    let query_text = format!("Instruct: {task}\nQuery: {query}");
    client.embed_text(&query_text).await
}

pub(crate) async fn embed_query(
    client: &ServerInferenceClient,
    task: &str,
    query: &str,
) -> Result<Vec<u8>> {
    let vec = embed_query_vec(client, task, query).await?;
    Ok(vec_to_blob(&vec))
}

pub(crate) fn project_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

pub(crate) const BACKGROUND_LOG_NAME: &str = ".inkentry/background.log";

pub(crate) fn background_log_path() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    inkentry_core::config::find_project_dir(&cwd).map(|d| d.join("background.log"))
}

// Append, not truncate: the two detached runs a commit fires share one file, and
// a fresh spawn must not clobber a child still writing at its own offset.
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

// Child output goes to the background log: a null sink would silence recurring
// failures, and inheriting the parent's streams would make a pipe reader
// (`git commit`, CI) block until the child exits, or SIGPIPE the child if it
// closes first. An unopenable log falls back to null so diagnostics never stop
// the work.
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

// `std` does not expose `O_NOFOLLOW`; avoids a `libc` dependency for one constant.
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

// `O_NOFOLLOW`: these files sit at predictable paths, and a pre-created symlink
// would turn the open into an overwrite primitive.
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

// `CreateProcessW` runs with `bInheritHandles = TRUE`, so the child inherits every
// inheritable handle, including a parent stdout pipe it never writes to, and a
// pipe reader sees no EOF until the child exits. Redirecting the child's streams
// does not help. Clearing `HANDLE_FLAG_INHERIT` for the span of the spawn
// prevents the copy; flags are restored on drop. Standard handles only, and not
// thread safe (the flag is process-wide). Inert on Unix.
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
