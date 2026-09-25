// State files under `inkentry_state_dir()` are also read by `capability/probe.rs`
// for loopback discovery, so both sides must go through that one resolver.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::color::cprintln;
use super::daemon_llm::LlmSpawn;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use inkentry_core::config::Config;

use crate::capability::inkentry_state_dir;

fn pid_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.pid")
}
fn port_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.port")
}
fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.log")
}
// The health body's id is self-reported and proves nothing alone: only a value
// we wrote down at start distinguishes our daemon from an impostor.
fn instance_id_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.instance_id")
}

pub(crate) fn read_instance_id(state_dir: &Path) -> Option<String> {
    let recorded = std::fs::read_to_string(instance_id_path(state_dir)).ok()?;
    let id = recorded.trim();
    (!id.is_empty()).then(|| id.to_string())
}

pub(super) fn create_state_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating state dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting 0700 permissions on {}", dir.display()))?;
    }
    Ok(())
}

pub(super) fn write_state_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut f = super::helpers::open_private_file_for_write(path)?;
    f.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn open_log_file_for_append(path: &Path) -> Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(super::helpers::libc_o_nofollow())
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }
}

pub(crate) fn read_pid(state_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path(state_dir))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

fn read_port(state_dir: &Path) -> Option<u16> {
    std::fs::read_to_string(port_path(state_dir))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

pub(super) fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0) checks existence without sending a signal.
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let rc = unsafe { kill(pid as i32, 0) };
        rc == 0
    }
    #[cfg(windows)]
    {
        // A NULL handle means no such process, or no access (treated as not alive).
        unsafe extern "system" {
            fn OpenProcess(desired_access: u32, inherit_handle: i32, pid: u32) -> *mut ();
            fn CloseHandle(handle: *mut ()) -> i32;
            fn GetExitCodeProcess(handle: *mut (), exit_code: *mut u32) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        unsafe { CloseHandle(handle) };
        ok != 0 && exit_code == STILL_ACTIVE
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Conservatively false so stale PIDs do not block a fresh start.
        let _ = pid;
        false
    }
}

const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(unix)]
const FORCE_KILL_TIMEOUT: Duration = Duration::from_secs(5);

// Records the DB the running daemon was started against, so a second `start`
// can refuse a different one.
fn db_path_file(state_dir: &Path) -> PathBuf {
    state_dir.join("server.db-path")
}

fn read_db_path(state_dir: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(db_path_file(state_dir))
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
}

fn same_path(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

// Identity signal for when `/v1/health` is silent: a wedged daemon is still an
// `inkentry-server` process and safe to kill, whereas a PID reused after a crash
// must not be.
pub(crate) fn process_matches_server(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "args="])
            .output()
        {
            Ok(out) if out.status.success() => {
                listing_names_server(&String::from_utf8_lossy(&out.stdout))
            }
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        {
            Ok(out) if out.status.success() => {
                listing_names_server(&String::from_utf8_lossy(&out.stdout))
            }
            _ => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

// Deliberately weak: a pre-rename `spelunk-server` fails it and any argv
// containing the string passes. `tasklist` renders the image name in its own
// case, hence the Windows case-fold.
#[cfg(unix)]
fn listing_names_server(listing: &str) -> bool {
    listing.contains("inkentry-server")
}
#[cfg(windows)]
fn listing_names_server(listing: &str) -> bool {
    listing.to_lowercase().contains("inkentry-server")
}

enum RunningServer {
    Healthy { port: u16 },
    HungOurs,
    // The PID was almost certainly reused after a crash; never signal it.
    Foreign,
}

// On no health response, falls back to the command-line identity check so a hung
// daemon is still recognised as ours and can be reclaimed.
async fn classify_running_server(state_dir: &Path, pid: u32) -> RunningServer {
    if let Some(port) = read_port(state_dir)
        && probe_health(port).await.is_some()
    {
        return RunningServer::Healthy { port };
    }
    if process_matches_server(pid) {
        return RunningServer::HungOurs;
    }
    RunningServer::Foreign
}

// Tolerates a process that already exited (`ESRCH`).
#[cfg(unix)]
fn force_kill(pid: u32) -> Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    let rc = unsafe { kill(pid as i32, SIGKILL) };
    if rc != 0 && pid_is_alive(pid) {
        anyhow::bail!("kill({pid}, SIGKILL) failed");
    }
    Ok(())
}

async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !pid_is_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    !pid_is_alive(pid)
}

async fn terminate_and_wait(pid: u32) -> Result<bool> {
    // An error because the process already exited (a race with classify) is success.
    if let Err(e) = terminate_process(pid) {
        if !pid_is_alive(pid) {
            return Ok(true);
        }
        return Err(e);
    }
    if wait_for_exit(pid, GRACEFUL_STOP_TIMEOUT).await {
        return Ok(true);
    }
    #[cfg(unix)]
    if pid_is_alive(pid) {
        force_kill(pid)?;
        if wait_for_exit(pid, FORCE_KILL_TIMEOUT).await {
            return Ok(true);
        }
    }
    Ok(!pid_is_alive(pid))
}

// Advisory `flock` held across a `start` so concurrent starts can't both spawn a
// daemon against the same state dir / DB.
#[cfg(unix)]
struct StartLock {
    _file: std::fs::File,
}

#[cfg(unix)]
fn acquire_start_lock(state_dir: &Path) -> Result<StartLock> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let path = state_dir.join("server.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc != 0 {
        anyhow::bail!(
            "another `inkentry server start` is already in progress for this machine. \
             Wait for it to finish, or check `inkentry server status`."
        );
    }
    Ok(StartLock { _file: file })
}

#[cfg(not(unix))]
struct StartLock;

#[cfg(not(unix))]
fn acquire_start_lock(_state_dir: &Path) -> Result<StartLock> {
    Ok(StartLock)
}

#[derive(Args, Debug)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: ServerCommand,
}

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Start a local inkentry-server daemon (idempotent)
    Start(ServerStartArgs),
    /// Stop the running local inkentry-server daemon
    Stop,
    /// Show status of the local inkentry-server daemon
    Status,
    /// Print the last N lines of the server log
    Logs(ServerLogsArgs),
}

#[derive(Args, Debug)]
pub struct ServerStartArgs {
    /// Port to bind. Explicit `start` does not drift to another port: if this
    /// one is held by an unrelated process, start fails loudly.
    #[arg(long, default_value_t = inkentry_core::config::DEFAULT_SERVER_PORT)]
    pub port: u16,

    /// Path to the inkentry-server binary (default: the `inkentry-server` in PATH)
    #[arg(long)]
    pub bin: Option<PathBuf>,

    /// Path to the server SQLite database (default: ~/.local/state/inkentry/server.db)
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Base URL of an OpenAI-compatible chat completions endpoint for this
    /// daemon. Overrides `INKENTRY_LLM_URL` and `llm_url` in the personal config.
    #[arg(long)]
    pub llm_url: Option<String>,

    /// LLM model name for this daemon. Overrides `INKENTRY_LLM_MODEL` and
    /// `llm_model` in the personal config.
    #[arg(long)]
    pub llm_model: Option<String>,
}

#[derive(Args, Debug)]
pub struct ServerLogsArgs {
    /// Number of lines to show (default: 50)
    #[arg(short = 'n', long, default_value = "50")]
    pub lines: usize,
}

pub async fn server(args: ServerArgs, cfg: Config) -> Result<()> {
    match args.command {
        ServerCommand::Start(a) => cmd_start(a, &cfg).await,
        ServerCommand::Stop => cmd_stop().await,
        ServerCommand::Status => cmd_status().await,
        ServerCommand::Logs(a) => cmd_logs(a),
    }
}

// Never spawns. A healthy answer on the recorded port proves nothing (any local
// process can hold it), so the responder must also match the recorded pid and
// `instance_id`, as loopback discovery requires before routing indexed work to it.
pub(crate) async fn probe_local_relay_port() -> Option<u16> {
    let state_dir = inkentry_state_dir().ok()?;
    let port = read_port(&state_dir)?;
    let health = probe_health(port).await?;
    if let Some(why) = crate::capability::untrusted_responder(health.instance_id.as_deref()) {
        // Loud on purpose: something answered and could not be verified, unlike the
        // ordinary no-daemon case.
        eprintln!(
            "warning: the process answering 127.0.0.1:{port} is not the server recorded in \
             {}: {why}. No memory entries or credentials were sent to it. If that is your \
             own daemon, run `inkentry server stop && inkentry server start`.",
            state_dir.display()
        );
        return None;
    }
    Some(port)
}

// Returns `(port, freshly_started)`; already healthy means `freshly_started = false`.
pub async fn ensure_server_running(start_port: u16, cfg: &Config) -> Result<(u16, bool)> {
    let state_dir = inkentry_state_dir()?;
    create_state_dir(&state_dir)?;

    let _start_lock = acquire_start_lock(&state_dir)?;

    // A wedged daemon must be reclaimed, not left running while we bind a different
    // port: that leaves two servers on one DB.
    if let Some(pid) = read_pid(&state_dir)
        && pid_is_alive(pid)
    {
        match classify_running_server(&state_dir, pid).await {
            RunningServer::Healthy { port } => return Ok((port, false)),
            RunningServer::HungOurs => {
                tracing::warn!(
                    "reclaiming unresponsive inkentry-server (pid={pid}) before restart"
                );
                let _ = terminate_and_wait(pid).await;
                cleanup_state_files(&state_dir);
            }
            RunningServer::Foreign => {
                cleanup_state_files(&state_dir);
            }
        }
    }

    let bin = which_inkentry_server()?;
    let db = state_dir.join("server.db");
    let port = find_available_port(start_port)?;

    let log_file = open_log_file_for_append(&log_path(&state_dir))?;
    let llm = LlmSpawn::resolve(cfg, None, None)?;

    #[cfg(unix)]
    let mut child = spawn_daemon_unix(&bin, &db, port, &llm, log_file)?;
    #[cfg(windows)]
    let mut child = spawn_daemon_windows(&bin, &db, port, &llm, log_file)?;

    let pid = child.id();
    write_state_file(&pid_path(&state_dir), &format!("{pid}\n")).context("writing server.pid")?;
    write_state_file(&port_path(&state_dir), &format!("{port}\n"))
        .context("writing server.port")?;
    write_state_file(&db_path_file(&state_dir), &format!("{}\n", db.display()))
        .context("writing server.db-path")?;
    // A leftover id from the previous daemon must not outlive it, even briefly.
    let _ = std::fs::remove_file(instance_id_path(&state_dir));

    // Liveness, not model readiness: health goes live at bind, before the model
    // download, so 30 s only bounds the give-up time.
    match wait_for_health(port, Duration::from_secs(30), &mut child).await {
        StartOutcome::Ready { instance_id } => record_instance_id(&state_dir, instance_id),
        StartOutcome::Exited(status) => {
            tracing::warn!(
                "inkentry-server (pid={pid}) exited immediately ({status}) instead of serving \
                 port {port}. It rejected its own startup configuration; the reason is the \
                 last line of `inkentry server logs`."
            );
        }
        StartOutcome::TimedOut => {
            // Alive and silent is what a blocked loopback listener looks like.
            tracing::warn!(
                "inkentry-server started (pid={pid}) but /v1/health did not respond within 30 s. \
                 A firewall may be blocking the local server (allow it, e.g. accept the Windows \
                 Defender Firewall prompt). Check `inkentry server logs`."
            );
        }
    }

    Ok((port, true))
}

async fn cmd_start(args: ServerStartArgs, cfg: &Config) -> Result<()> {
    let state_dir = inkentry_state_dir()?;
    create_state_dir(&state_dir)?;

    let _start_lock = acquire_start_lock(&state_dir)?;

    let db = args
        .db
        .clone()
        .unwrap_or_else(|| state_dir.join("server.db"));

    // Falling through to a new port for an alive-but-unhealthy daemon would leave
    // it holding the old one: two servers on one DB.
    if let Some(pid) = read_pid(&state_dir) {
        if pid_is_alive(pid) {
            match classify_running_server(&state_dir, pid).await {
                RunningServer::Healthy { port } => {
                    // The state dir tracks one daemon; clobbering it would orphan
                    // the running one.
                    if let Some(running_db) = read_db_path(&state_dir)
                        && !same_path(&running_db, &db)
                    {
                        anyhow::bail!(
                            "a inkentry-server is already running (pid={pid}, port={port}) against \
                             {}. Stop it first with `inkentry server stop` before starting one \
                             against {}.",
                            running_db.display(),
                            db.display()
                        );
                    }
                    println!("inkentry-server is already running (pid={pid}, port={port}).");
                    return Ok(());
                }
                RunningServer::HungOurs => {
                    println!("Reclaiming unresponsive inkentry-server (pid={pid})...");
                    if !terminate_and_wait(pid).await? {
                        anyhow::bail!(
                            "could not stop the unresponsive inkentry-server (pid={pid}); it \
                             survived SIGTERM and SIGKILL. Kill it manually and retry."
                        );
                    }
                    cleanup_state_files(&state_dir);
                }
                RunningServer::Foreign => {
                    tracing::warn!(
                        "recorded pid={pid} is not a inkentry-server (PID reused); clearing stale state"
                    );
                    cleanup_state_files(&state_dir);
                }
            }
        } else {
            cleanup_state_files(&state_dir);
        }
    }

    let bin = match &args.bin {
        Some(p) => {
            if !p.exists() {
                anyhow::bail!("inkentry-server binary not found at {}", p.display());
            }
            p.clone()
        }
        None => which_inkentry_server()?,
    };

    // Any wedged daemon of ours was reclaimed above, so an occupied port belongs to
    // an unrelated process: fail loudly rather than bind elsewhere.
    let port = args.port;
    ensure_port_available_for_start(port).await?;

    let log_file = open_log_file_for_append(&log_path(&state_dir))?;
    let llm = LlmSpawn::resolve(cfg, args.llm_url.as_deref(), args.llm_model.as_deref())?;

    #[cfg(unix)]
    let mut child = spawn_daemon_unix(&bin, &db, port, &llm, log_file)?;
    #[cfg(windows)]
    let mut child = spawn_daemon_windows(&bin, &db, port, &llm, log_file)?;

    let pid = child.id();

    write_state_file(&pid_path(&state_dir), &format!("{pid}\n")).context("writing server.pid")?;
    write_state_file(&port_path(&state_dir), &format!("{port}\n"))
        .context("writing server.port")?;
    write_state_file(&db_path_file(&state_dir), &format!("{}\n", db.display()))
        .context("writing server.db-path")?;
    let _ = std::fs::remove_file(instance_id_path(&state_dir));

    match wait_for_health(port, Duration::from_secs(30), &mut child).await {
        StartOutcome::Ready { instance_id } => {
            record_instance_id(&state_dir, instance_id);
            println!("inkentry-server started (pid={pid}, port={port}).");
            println!("  Log: {}", log_path(&state_dir).display());
        }
        StartOutcome::Exited(status) => {
            eprintln!(
                "warning: inkentry-server (pid={pid}) exited immediately ({status}) instead of \
                 serving port {port}. It rejected its own startup configuration; the reason is \
                 the last line of the log: {}",
                log_path(&state_dir).display()
            );
        }
        StartOutcome::TimedOut => {
            // Alive and silent is the only case a firewall explains.
            eprintln!(
                "warning: inkentry-server process started (pid={pid}) but /v1/health did not \
                 respond on port {port} within 30 s. A firewall may be blocking the local \
                 server (allow it, e.g. accept the Windows Defender Firewall prompt). Check \
                 the log: {}",
                log_path(&state_dir).display()
            );
        }
    }

    Ok(())
}

// The sibling binary wins over `$PATH` so a hostile `inkentry-server` earlier on
// `$PATH` (or in an untrusted repo's tooling dir) is never run. Other tools (git,
// gh, editors) use bare names by convention; this one is first-party and
// auto-spawned without the user typing a command.
fn which_inkentry_server() -> Result<PathBuf> {
    #[cfg(windows)]
    let bin_name = "inkentry-server.exe";
    #[cfg(not(windows))]
    let bin_name = "inkentry-server";

    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(bin_name);
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(bin_name))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "inkentry-server binary not found. \
                 Install it alongside `inkentry` or pass --bin <path>."
            )
        })
}

// Never drifts to another port: a silent drift leaves a stale daemon on the old
// one. The retry absorbs the window while the OS releases a reclaimed daemon's socket.
async fn ensure_port_available_for_start(port: u16) -> Result<()> {
    for attempt in 0..10 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        if attempt < 9 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    anyhow::bail!(
        "port {port} is already in use by another process. If it is a inkentry-server not \
         managed here, stop it; otherwise free the port or pass `--port <N>`."
    );
}

// Nothing needs the daemon's port to be predictable: `server.port` records what
// was bound and loopback discovery reads it before falling back to the default.
fn find_available_port(preferred: u16) -> Result<u16> {
    if std::net::TcpListener::bind(("127.0.0.1", preferred)).is_ok() {
        return Ok(preferred);
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("binding an ephemeral loopback port for the auto-started server")?;
    Ok(listener.local_addr()?.port())
}

// `--host 127.0.0.1` is always explicit: the auto-spawned daemon is
// unauthenticated, so it must only bind loopback regardless of the server's
// default. `llm` contributes only non-secret values; its credential travels in
// the child environment so no key lands in the world-readable process table.
pub(super) fn build_daemon_args(db: &Path, port: u16, llm: &LlmSpawn) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
        "--db".into(),
        db.as_os_str().into(),
    ];
    args.extend(llm.args());
    args
}

// Removing a variable is as load-bearing as setting one: `inkentry-server` reads
// `INKENTRY_LLM_URL`/`INKENTRY_LLM_MODEL` through clap `env`, so anything left
// inherited is a value this process already decided against.
fn apply_llm_child_env(cmd: &mut std::process::Command, llm: &LlmSpawn) {
    for (name, value) in llm.child_env() {
        match value {
            Some(v) => cmd.env(name, v),
            None => cmd.env_remove(name),
        };
    }
}

// `setsid()` matters as much as reparenting to init: without it the daemon stays
// in the spawning shell's session and dies with that terminal's SIGHUP, which is
// the common case for a short-lived shell (`ssh host 'inkentry server start'`).
#[cfg(unix)]
fn spawn_daemon_unix(
    bin: &Path,
    db: &Path,
    port: u16,
    llm: &LlmSpawn,
    log_file: std::fs::File,
) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;

    let log_file_err = log_file.try_clone().context("cloning log file handle")?;

    let mut cmd = std::process::Command::new(bin);
    for arg in build_daemon_args(db, port, llm) {
        cmd.arg(arg);
    }
    apply_llm_child_env(&mut cmd, llm);
    // SAFETY: runs in the forked child before `exec`, where only
    // async-signal-safe calls are permitted; `setsid` is one.
    unsafe {
        cmd.pre_exec(|| {
            unsafe extern "C" {
                fn setsid() -> i32;
            }
            // EPERM (already a group leader) is the only documented failure and a
            // just-forked child cannot hit it; fail rather than start a daemon that
            // dies with the terminal.
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_file_err)
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;

    Ok(child)
}

// Counterpart to `setsid()`. `CREATE_NEW_PROCESS_GROUP` only stops Ctrl-C from
// reaching the daemon; it stays on the console and dies on `CTRL_CLOSE_EVENT`, so
// `DETACHED_PROCESS` is also needed. That is safe only because stdio is fully
// redirected below; changing that reintroduces the failure.
#[cfg(windows)]
fn spawn_daemon_windows(
    bin: &Path,
    db: &Path,
    port: u16,
    llm: &LlmSpawn,
    log_file: std::fs::File,
) -> Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let mut cmd = std::process::Command::new(bin);
    for arg in build_daemon_args(db, port, llm) {
        cmd.arg(arg);
    }
    apply_llm_child_env(&mut cmd, llm);
    let _std_handles = super::helpers::StdHandlesNotInherited::for_spawn();
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS)
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;

    Ok(child)
}

enum StartOutcome {
    Ready { instance_id: Option<String> },
    // The process is gone, so whatever went wrong is not the network.
    Exited(std::process::ExitStatus),
    TimedOut,
}

// Watching the child separates "nothing can reach the listener" from "there is no
// listener": a daemon that refused its configuration exits in milliseconds, and
// blaming a firewall after a 30 s wait sends the user to the wrong place.
async fn wait_for_health(
    port: u16,
    timeout: Duration,
    child: &mut std::process::Child,
) -> StartOutcome {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Some(health) = probe_health(port).await {
            return StartOutcome::Ready {
                instance_id: health.instance_id,
            };
        }
        // After the probe, so a daemon that answers and then exits in the same tick
        // still counts as started.
        if let Ok(Some(status)) = child.try_wait() {
            return StartOutcome::Exited(status);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    StartOutcome::TimedOut
}

// `instance_id` stays `Option` inside a `Some`: a server that answered but named
// no instance is alive and has nothing worth recording.
struct HealthIdentity {
    instance_id: Option<String>,
}

async fn probe_health(port: u16) -> Option<HealthIdentity> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .ok()?;
    let url = format!("http://127.0.0.1:{port}/v1/health");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct H {
        instance_id: Option<String>,
    }
    let body: H = resp.json().await.ok()?;
    Some(HealthIdentity {
        instance_id: body.instance_id.filter(|id| !id.trim().is_empty()),
    })
}

async fn cmd_stop() -> Result<()> {
    let state_dir = inkentry_state_dir()?;
    let pid = read_pid(&state_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "no server.pid in {} — this CLI has no record of a running inkentry-server. If one \
             is running anyway, it was not started from here (or its state files were removed): \
             find it with `ps ax | grep inkentry-server` and stop that process directly.",
            state_dir.display()
        )
    })?;

    if !pid_is_alive(pid) {
        println!("inkentry-server (pid={pid}) is not running. Cleaning up state files.");
        cleanup_state_files(&state_dir);
        return Ok(());
    }

    // Liveness alone is not enough (PIDs are reused) and health alone is too strict
    // (a wedged daemon's health is silent): healthy or hung-but-ours is ours to
    // kill; only a foreign process is refused.
    match classify_running_server(&state_dir, pid).await {
        RunningServer::Healthy { .. } | RunningServer::HungOurs => {}
        RunningServer::Foreign => {
            anyhow::bail!(
                "refusing to stop pid={pid}: it does not look like the inkentry-server recorded \
                 in {}. If the server crashed and this PID was reused by an unrelated process, \
                 remove the stale state files manually (under {}) and retry.",
                pid_path(&state_dir).display(),
                state_dir.display()
            );
        }
    }

    if terminate_and_wait(pid).await? {
        println!("inkentry-server stopped.");
        cleanup_state_files(&state_dir);
        Ok(())
    } else {
        anyhow::bail!(
            "inkentry-server (pid={pid}) is still running after SIGTERM and SIGKILL. State files \
             left in place; retry `inkentry server stop` or kill the process manually."
        );
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        let rc = unsafe { kill(pid as i32, SIGTERM) };
        if rc != 0 {
            anyhow::bail!("kill({pid}, SIGTERM) failed");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()
            .context("running taskkill")?;
        if !status.success() {
            anyhow::bail!("taskkill /PID {pid} /F failed");
        }
        Ok(())
    }
}

// A daemon that named no instance leaves no file, so loopback discovery declines
// it: it cannot be told apart from anything else holding the port. A write
// failure is not fatal, since the daemon is up; it only costs auto-discovery.
fn record_instance_id(state_dir: &Path, instance_id: Option<String>) {
    let Some(id) = instance_id else {
        tracing::warn!(
            "inkentry-server did not report an instance_id, so nothing was recorded to \
             identify it by; loopback auto-discovery will not use this server"
        );
        return;
    };
    if let Err(e) = write_state_file(&instance_id_path(state_dir), &format!("{id}\n")) {
        tracing::warn!(
            "could not record the server's instance_id ({e}); loopback auto-discovery \
             will not use this server"
        );
    }
}

fn cleanup_state_files(state_dir: &Path) {
    let _ = std::fs::remove_file(pid_path(state_dir));
    let _ = std::fs::remove_file(port_path(state_dir));
    let _ = std::fs::remove_file(db_path_file(state_dir));
    let _ = std::fs::remove_file(instance_id_path(state_dir));
}

async fn cmd_status() -> Result<()> {
    let state_dir = inkentry_state_dir()?;
    let pid = read_pid(&state_dir);
    let port = read_port(&state_dir);

    match (pid, port) {
        (Some(pid), Some(port)) if pid_is_alive(pid) => {
            cprintln!("inkentry-server  \x1b[32mrunning\x1b[0m");
            println!("  PID:   {pid}");
            println!("  Port:  {port}");
            println!("  Log:   {}", log_path(&state_dir).display());

            match probe_health_verbose(port).await {
                Some(info) => {
                    println!("  URL:   http://127.0.0.1:{port}");
                    if let Some(id) = info.instance_id {
                        println!("  ID:    {id}");
                    }
                    if let Some(ver) = info.version {
                        println!("  Ver:   {ver}");
                    }
                    if let Some(engine) = info.engine {
                        println!("  Engine:{engine}");
                    }
                    if let Some(device) = info.device {
                        println!("  Device:{device}");
                    }
                    // Highlighted because it explains a `cpu` device and is user-fixable
                    // (e.g. GPU blocked by a missing `render`-group membership).
                    if let Some(note) = info.note {
                        cprintln!("  \x1b[33mNote:  {note}\x1b[0m");
                    }
                }
                None => {
                    cprintln!("  URL:   http://127.0.0.1:{port}  \x1b[31m(unreachable)\x1b[0m");
                }
            }
        }
        (Some(pid), _) if pid_is_alive(pid) => {
            cprintln!("inkentry-server  \x1b[33mrunning\x1b[0m (port unknown)");
            println!("  PID: {pid}");
        }
        (Some(pid), _) => {
            cprintln!("inkentry-server  \x1b[31mstopped\x1b[0m (stale pid={pid})");
            println!("  Run `inkentry server start` to start.");
        }
        (None, _) => {
            cprintln!("inkentry-server  \x1b[31mnot started\x1b[0m");
            println!("  Run `inkentry server start` to start.");
        }
    }
    Ok(())
}

struct HealthInfo {
    instance_id: Option<String>,
    version: Option<String>,
    engine: Option<String>,
    device: Option<String>,
    // Only while `ready`, where `detail` is a hint, never an `unavailable`
    // embedder's load-failure error.
    note: Option<String>,
}

async fn probe_health_verbose(port: u16) -> Option<HealthInfo> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let url = format!("http://127.0.0.1:{port}/v1/health");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct Embedder {
        state: Option<String>,
        detail: Option<String>,
        engine: Option<String>,
        device: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct H {
        instance_id: Option<String>,
        version: Option<String>,
        embedder: Option<Embedder>,
    }
    let body: H = resp.json().await.ok()?;
    let embedder = body.embedder;
    let ready = embedder
        .as_ref()
        .and_then(|e| e.state.as_deref())
        .is_some_and(|s| s == "ready");
    Some(HealthInfo {
        instance_id: body.instance_id,
        version: body.version,
        engine: embedder.as_ref().and_then(|e| e.engine.clone()),
        device: embedder.as_ref().and_then(|e| e.device.clone()),
        note: ready.then(|| embedder.and_then(|e| e.detail)).flatten(),
    })
}

fn cmd_logs(args: ServerLogsArgs) -> Result<()> {
    let state_dir = inkentry_state_dir()?;
    let log = log_path(&state_dir);

    if !log.exists() {
        anyhow::bail!(
            "No log file at {}. Start the server first with `inkentry server start`.",
            log.display()
        );
    }

    let content =
        std::fs::read_to_string(&log).with_context(|| format!("reading {}", log.display()))?;

    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(args.lines);
    for line in &lines[start..] {
        println!("{line}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    #[test]
    #[serial(server_state_dir_env)]
    fn state_dir_contains_inkentry() {
        let dir = inkentry_state_dir().expect("state dir");
        assert!(
            dir.to_string_lossy().contains("inkentry"),
            "state dir should contain 'inkentry', got {dir:?}"
        );
    }

    // A leaked instance-id file outlives its daemon, and the next probe then
    // refuses a live server for not matching the dead one's id.
    #[test]
    fn cleanup_removes_every_recorded_state_file() {
        let dir = TempDir::new().expect("state dir");
        let files = [
            pid_path(dir.path()),
            port_path(dir.path()),
            db_path_file(dir.path()),
            instance_id_path(dir.path()),
        ];
        for f in &files {
            std::fs::write(f, "stale").expect("seed state file");
        }

        cleanup_state_files(dir.path());

        for f in &files {
            assert!(!f.exists(), "{} survived cleanup", f.display());
        }
    }

    #[test]
    fn find_available_port_returns_the_preferred_port_when_free() {
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let free = probe.local_addr().unwrap().port();
        drop(probe);

        assert_eq!(find_available_port(free).unwrap(), free);
    }

    #[test]
    fn find_available_port_falls_back_to_an_ephemeral_port_when_taken() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = held.local_addr().unwrap().port();

        let port = find_available_port(taken).expect("must fall back, not fail");
        assert_ne!(port, taken);
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "fallback port {port} must be bindable"
        );
    }

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(pid_is_alive(pid), "current process should be alive");
    }

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        assert!(read_pid(tmp.path()).is_none());
    }

    #[test]
    fn read_pid_round_trips() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(pid_path(tmp.path()), b"12345\n").unwrap();
        assert_eq!(read_pid(tmp.path()), Some(12345));
    }

    #[test]
    fn read_port_round_trips() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(port_path(tmp.path()), b"4655\n").unwrap();
        assert_eq!(read_port(tmp.path()), Some(4655));
    }

    struct PathGuard(std::ffi::OsString);

    impl PathGuard {
        fn capture() -> Self {
            PathGuard(std::env::var_os("PATH").unwrap_or_default())
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: the `#[serial(path_env)]` attribute guarantees no other
            // test that reads or writes `PATH` runs concurrently.
            unsafe { std::env::set_var("PATH", &self.0) };
        }
    }

    // These tests mutate the process-global `PATH`, and `DummyProc` spawns resolve
    // `sleep` through it, so an empty PATH from a sibling would fail the spawn with
    // ENOENT; all share the `path_env` serial group.

    #[test]
    #[serial(path_env)]
    fn which_inkentry_server_finds_sibling_binary() {
        let tmp = TempDir::new().unwrap();
        #[cfg(windows)]
        let fake_bin = tmp.path().join("inkentry-server.exe");
        #[cfg(not(windows))]
        let fake_bin = tmp.path().join("inkentry-server");
        std::fs::write(&fake_bin, b"").unwrap();

        // `current_exe()` can't be overridden, so this exercises only the PATH fallback.
        //
        // SAFETY: `#[serial(path_env)]` serialises this test against every other
        // PATH-mutating test, so no other thread reads or writes PATH while this
        // runs. The `PathGuard` restores PATH even if the assertion below panics.
        let _guard = PathGuard::capture();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        #[cfg(windows)]
        let new_path = format!("{};{}", tmp.path().display(), old_path.to_string_lossy());
        #[cfg(not(windows))]
        let new_path = format!("{}:{}", tmp.path().display(), old_path.to_string_lossy());
        unsafe { std::env::set_var("PATH", &new_path) };
        let result = which_inkentry_server();

        assert!(result.is_ok(), "should discover binary on PATH: {result:?}");
    }

    #[test]
    #[serial(path_env)]
    fn which_inkentry_server_fails_when_not_on_path() {
        // SAFETY: see note in which_inkentry_server_finds_sibling_binary; the
        // `#[serial(path_env)]` group serialises this against the sibling test,
        // and the `PathGuard` restores PATH even if the assertion panics.
        let _guard = PathGuard::capture();
        unsafe { std::env::set_var("PATH", "") };
        let result = which_inkentry_server();
        assert!(result.is_err(), "should fail when binary is not on PATH");
    }

    // The auto-spawned daemon is unauthenticated, so `build_daemon_args` (shared by
    // both spawn helpers) must pin it to loopback.

    #[test]
    fn spawn_daemon_args_bind_loopback() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let args = build_daemon_args(&db, 4655, &LlmSpawn::default());

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(
            args_str.contains(&"--host".to_string()),
            "daemon must bind loopback: --host flag missing from daemon args: {args_str:?}"
        );

        let host_idx = args_str
            .iter()
            .position(|a| a == "--host")
            .expect("--host must be present");
        let host_value = args_str
            .get(host_idx + 1)
            .expect("--host must be followed by a value");
        assert_eq!(
            host_value, "127.0.0.1",
            "daemon must bind 127.0.0.1 only, got {host_value:?}"
        );
    }

    #[test]
    fn spawn_daemon_args_do_not_bind_wildcard() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let args = build_daemon_args(&db, 4655, &LlmSpawn::default());

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(
            !args_str.contains(&"0.0.0.0".to_string()),
            "daemon args must not contain 0.0.0.0 (wildcard bind): {args_str:?}"
        );
    }

    #[test]
    fn spawn_daemon_args_include_port() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let port: u16 = 7780;
        let args = build_daemon_args(&db, port, &LlmSpawn::default());

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let port_idx = args_str
            .iter()
            .position(|a| a == "--port")
            .expect("--port must be present in daemon args");
        let port_value = args_str
            .get(port_idx + 1)
            .expect("--port must be followed by a value");
        assert_eq!(
            port_value,
            &port.to_string(),
            "daemon arg --port value should match requested port"
        );
    }

    #[test]
    fn spawn_daemon_args_include_db_path() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("server.db");
        let args = build_daemon_args(&db, 4655, &LlmSpawn::default());

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let db_idx = args_str
            .iter()
            .position(|a| a == "--db")
            .expect("--db must be present in daemon args");
        let db_value = args_str
            .get(db_idx + 1)
            .expect("--db must be followed by a value");
        assert_eq!(
            db_value,
            &db.to_string_lossy().into_owned(),
            "daemon arg --db value should match supplied db path"
        );
    }

    #[test]
    fn spawn_daemon_args_without_llm_are_host_port_db_only() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("server.db");
        let args = build_daemon_args(&db, 4655, &LlmSpawn::default());

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args_str,
            vec![
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "4655".to_string(),
                "--db".to_string(),
                db.to_string_lossy().into_owned(),
            ]
        );
    }

    #[test]
    fn spawn_daemon_args_carry_the_llm_url_and_model_but_never_the_key() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("server.db");
        let llm = LlmSpawn {
            url: Some("https://gateway.example".to_string()),
            model: Some("gpt-oss".to_string()),
            key: Some("sk-llm-secret".to_string()),
        };
        let args = build_daemon_args(&db, 4655, &llm);

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(args_str.contains(&"--llm-url".to_string()));
        assert!(args_str.contains(&"https://gateway.example".to_string()));
        assert!(args_str.contains(&"--llm-model".to_string()));
        assert!(args_str.contains(&"gpt-oss".to_string()));
        assert!(
            args_str.iter().all(|a| !a.contains("sk-llm-secret")),
            "the LLM credential must never reach the process table: {args_str:?}"
        );
    }

    // The parent carries no INKENTRY_LLM_KEY, so only an explicit `cmd.env` on the
    // child can deliver the credential.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn the_spawned_child_receives_the_key_in_its_environment_and_never_in_argv() {
        use std::os::unix::fs::PermissionsExt;

        // SAFETY: pinned to the `path_env` serial group, which is the only
        // group in this module that mutates process-global environment.
        unsafe { std::env::remove_var("INKENTRY_LLM_KEY") };

        let tmp = TempDir::new().unwrap();
        let record = tmp.path().join("record.txt");
        let fake_server = tmp.path().join("fake-inkentry-server");
        std::fs::write(
            &fake_server,
            format!(
                "#!/bin/sh\n{{ echo \"ARGV $*\"; env; }} > '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_server, std::fs::Permissions::from_mode(0o755)).unwrap();

        let llm = LlmSpawn {
            url: Some("https://gateway.example".to_string()),
            model: Some("gpt-oss".to_string()),
            key: Some("sk-llm-secret".to_string()),
        };
        let log = std::fs::File::create(tmp.path().join("server.log")).unwrap();
        let mut child =
            spawn_daemon_unix(&fake_server, &tmp.path().join("server.db"), 4655, &llm, log)
                .expect("spawning the recording stand-in");
        child.wait().unwrap();

        let recorded = std::fs::read_to_string(&record).unwrap();
        let argv = recorded.lines().next().unwrap_or_default().to_string();

        assert!(
            recorded
                .lines()
                .any(|l| l == "INKENTRY_LLM_KEY=sk-llm-secret"),
            "the credential must reach the child environment: {recorded}"
        );
        assert!(
            argv.contains("--llm-url https://gateway.example"),
            "the endpoint belongs in argv: {argv}"
        );
        assert!(
            !argv.contains("sk-llm-secret"),
            "the credential must never reach the process table: {argv}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn a_keyless_spawn_adds_no_key_entry_to_the_child_environment() {
        use std::os::unix::fs::PermissionsExt;

        // SAFETY: see the sibling test; same serial group.
        unsafe { std::env::remove_var("INKENTRY_LLM_KEY") };

        let tmp = TempDir::new().unwrap();
        let record = tmp.path().join("record.txt");
        let fake_server = tmp.path().join("fake-inkentry-server");
        std::fs::write(
            &fake_server,
            format!("#!/bin/sh\nenv > '{}'\n", record.display()),
        )
        .unwrap();
        std::fs::set_permissions(&fake_server, std::fs::Permissions::from_mode(0o755)).unwrap();

        let log = std::fs::File::create(tmp.path().join("server.log")).unwrap();
        let mut child = spawn_daemon_unix(
            &fake_server,
            &tmp.path().join("server.db"),
            4655,
            &LlmSpawn::default(),
            log,
        )
        .expect("spawning the recording stand-in");
        child.wait().unwrap();

        let recorded = std::fs::read_to_string(&record).unwrap();
        assert!(
            !recorded.contains("INKENTRY_LLM_KEY"),
            "no credential resolved, so none should have been set: {recorded}"
        );
    }

    #[cfg(windows)]
    struct KillOnDropWindows(std::process::Child);

    #[cfg(windows)]
    impl Drop for KillOnDropWindows {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Every process in the console's list receives `CTRL_CLOSE_EVENT` when it
    // closes. Both spawns use the real binary and differ only in creation flags;
    // the control must be alive and listed first, or the assertion is vacuous
    // (`GetConsoleProcessList` is empty without a console, and an exited child is
    // absent for unrelated reasons).
    #[cfg(windows)]
    #[test]
    fn the_spawned_daemon_is_not_attached_to_our_console() {
        unsafe extern "system" {
            fn GetConsoleProcessList(lpdwProcessList: *mut u32, dwProcessCount: u32) -> u32;
        }

        fn console_pids() -> Vec<u32> {
            let mut buf = vec![0u32; 256];
            let n = unsafe { GetConsoleProcessList(buf.as_mut_ptr(), buf.len() as u32) };
            buf.truncate(n as usize);
            buf
        }

        fn free_port() -> u16 {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        }

        // `target/<profile>/deps/<test>.exe` -> `target/<profile>/inkentry-server.exe`.
        let server_bin = std::env::current_exe()
            .unwrap()
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("inkentry-server.exe"));
        let Some(bin) = server_bin.filter(|p| p.exists()) else {
            eprintln!("skipping: inkentry-server.exe is not built alongside this test");
            return;
        };

        let tmp = TempDir::new().unwrap();

        let control = std::process::Command::new(&bin)
            .args(build_daemon_args(
                &tmp.path().join("control.db"),
                free_port(),
                &LlmSpawn::default(),
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawning the control daemon");
        let control_pid = control.id();
        let mut control_guard = KillOnDropWindows(control);

        std::thread::sleep(Duration::from_millis(500));
        if control_guard.0.try_wait().unwrap().is_some() {
            eprintln!("skipping: the control daemon exited before it could be observed");
            return;
        }
        if !console_pids().contains(&control_pid) {
            eprintln!(
                "skipping: this test process has no console, so console attachment \
                 cannot be observed and the assertion below would be vacuous"
            );
            return;
        }

        let log = std::fs::File::create(tmp.path().join("server.log")).unwrap();
        let child = spawn_daemon_windows(
            &bin,
            &tmp.path().join("server.db"),
            free_port(),
            &LlmSpawn::default(),
            log,
        )
        .expect("spawning the detached daemon");
        let pid = child.id();
        let mut guard = KillOnDropWindows(child);

        std::thread::sleep(Duration::from_millis(500));
        assert!(
            guard.0.try_wait().unwrap().is_none(),
            "the detached daemon exited on its own, so its absence from the \
             console list would prove nothing"
        );
        assert!(
            !console_pids().contains(&pid),
            "the daemon is attached to the spawning console, so closing that \
             console would deliver CTRL_CLOSE_EVENT and take the daemon down"
        );
    }

    // The child is in a session of its own, so nothing that signals this test's
    // process group would reach it; SIGKILL on drop is the only cleanup.
    #[cfg(unix)]
    struct KillOnDrop(std::process::Child);

    #[cfg(unix)]
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Asserts the mechanism, not the consequence: closing a terminal can't be
    // staged. `spawn` returns after exec, so the sid read is post-`setsid`.
    #[cfg(unix)]
    #[test]
    fn the_spawned_daemon_leads_a_session_of_its_own() {
        use std::os::unix::fs::PermissionsExt;

        unsafe extern "C" {
            fn getsid(pid: i32) -> i32;
        }

        let tmp = TempDir::new().unwrap();
        let bin = tmp.path().join("fake-inkentry-server");
        // `exec` so the sleeping process is the one that was spawned: a shell
        // wrapper would leave `sleep` behind when the guard kills the shell.
        std::fs::write(&bin, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let log = std::fs::File::create(tmp.path().join("server.log")).unwrap();
        let child = spawn_daemon_unix(
            &bin,
            &tmp.path().join("server.db"),
            4655,
            &LlmSpawn::default(),
            log,
        )
        .expect("spawning the stand-in");
        let pid = child.id() as i32;
        let _guard = KillOnDrop(child);

        let ours = unsafe { getsid(0) };
        let theirs = unsafe { getsid(pid) };
        assert!(theirs > 0, "could not read the spawned daemon's session id");
        assert_ne!(
            theirs, ours,
            "the daemon stayed in the spawning process's session, so closing that \
             terminal would take it down with the shell"
        );
        assert_eq!(
            theirs, pid,
            "the daemon should lead its own session (sid == pid)"
        );
    }

    #[cfg(unix)]
    const LLM_ENV: [&str; 3] = ["INKENTRY_LLM_URL", "INKENTRY_LLM_MODEL", "INKENTRY_LLM_KEY"];

    #[cfg(unix)]
    struct LlmEnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    #[cfg(unix)]
    impl LlmEnvGuard {
        // So whatever a spawned child ends up with can only come from what the
        // code under test resolved.
        fn isolated(config_dir: &Path) -> Self {
            let names = ["INKENTRY_SECRET_STORE", "INKENTRY_CONFIG_DIR"];
            let saved = LLM_ENV
                .iter()
                .chain(names.iter())
                .map(|n| (*n, std::env::var_os(n)))
                .collect();
            // SAFETY: every user of this guard is in the `path_env` serial
            // group, which is the only group in this module mutating
            // process-global environment.
            unsafe {
                for name in LLM_ENV {
                    std::env::remove_var(name);
                }
                std::env::set_var("INKENTRY_SECRET_STORE", "file");
                std::env::set_var("INKENTRY_CONFIG_DIR", config_dir);
            }
            Self(saved)
        }

        fn export(&self, name: &str, value: &str) {
            // SAFETY: see `isolated`.
            unsafe { std::env::set_var(name, value) };
        }
    }

    #[cfg(unix)]
    impl Drop for LlmEnvGuard {
        fn drop(&mut self) {
            // SAFETY: see `isolated`.
            unsafe {
                for (name, prev) in &self.0 {
                    match prev {
                        Some(v) => std::env::set_var(name, v),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    fn recording_server_named(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let record = dir.join("record.txt");
        let bin = dir.join(name);
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\n{{ echo \"ARGV $*\"; env; }} > '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        record
    }

    // The server reads these through clap `env`, so a resolved-away variable must be
    // cleared on the child. An exported empty endpoint shows it: inheriting hands
    // the daemon a present-but-empty one.
    #[cfg(unix)]
    #[test]
    #[serial(path_env)]
    fn a_spawn_that_resolved_nothing_clears_the_inherited_llm_variables() {
        let tmp = TempDir::new().unwrap();
        let record = recording_server_named(tmp.path(), "fake-inkentry-server");
        let guard = LlmEnvGuard::isolated(tmp.path());
        guard.export("INKENTRY_LLM_URL", "");
        guard.export("INKENTRY_LLM_MODEL", "stale-model");
        guard.export("INKENTRY_LLM_KEY", "");

        let log = std::fs::File::create(tmp.path().join("server.log")).unwrap();
        let mut child = spawn_daemon_unix(
            &tmp.path().join("fake-inkentry-server"),
            &tmp.path().join("server.db"),
            4655,
            &LlmSpawn::default(),
            log,
        )
        .expect("spawning the recording stand-in");
        child.wait().unwrap();

        let recorded = std::fs::read_to_string(&record).unwrap();
        for name in LLM_ENV {
            assert!(
                !recorded.lines().any(|l| l.starts_with(&format!("{name}="))),
                "{name} was inherited by the child although nothing resolved it: {recorded}"
            );
        }
    }

    // Drives `ensure_server_running` itself so dropping the resolution at that call
    // site cannot stay green.
    #[cfg(unix)]
    #[tokio::test]
    #[serial(path_env, server_state_dir_env)]
    async fn ensure_server_running_hands_the_configured_endpoint_to_the_daemon() {
        let tmp = TempDir::new().unwrap();
        let record = recording_server_named(tmp.path(), "inkentry-server");

        let _path = PathGuard::capture();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        // SAFETY: `#[serial(path_env, ...)]` serialises this against every
        // other PATH-mutating test; `PathGuard` restores PATH on panic.
        unsafe {
            std::env::set_var(
                "PATH",
                format!("{}:{}", tmp.path().display(), old_path.to_string_lossy()),
            )
        };
        let _state = StateDirGuard::set(&tmp.path().join("state"));
        let _env = LlmEnvGuard::isolated(tmp.path());

        let cfg = Config {
            llm_url: Some("http://endpoint.invalid:1234".to_string()),
            llm_model: Some("from-config".to_string()),
            ..Config::default()
        };
        let (_port, freshly_started) = ensure_server_running(19800, &cfg)
            .await
            .expect("auto-start against the recording stand-in");
        assert!(freshly_started);

        let recorded = std::fs::read_to_string(&record)
            .unwrap_or_else(|e| panic!("the daemon stand-in recorded nothing ({e})"));
        let argv = recorded.lines().next().unwrap_or_default();
        assert!(
            argv.contains("--llm-url http://endpoint.invalid:1234"),
            "the configured endpoint never reached the auto-started daemon: {argv}"
        );
        assert!(
            argv.contains("--llm-model from-config"),
            "the configured model never reached the auto-started daemon: {argv}"
        );
    }

    struct StateDirGuard(Option<std::ffi::OsString>);
    impl StateDirGuard {
        fn set(dir: &Path) -> Self {
            let prev = std::env::var_os("INKENTRY_STATE_DIR");
            unsafe { std::env::set_var("INKENTRY_STATE_DIR", dir) };
            Self(prev)
        }
    }
    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            // SAFETY: `#[serial(server_state_dir_env)]` on every test using
            // this guard serialises against all others touching the var.
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                    None => std::env::remove_var("INKENTRY_STATE_DIR"),
                }
            }
        }
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn probe_local_relay_port_none_when_no_state_dir_at_all() {
        let tmp = TempDir::new().unwrap();
        let _guard = StateDirGuard::set(&tmp.path().join("nonexistent"));
        assert_eq!(probe_local_relay_port().await, None);
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn probe_local_relay_port_none_when_port_file_present_but_unhealthy() {
        let tmp = TempDir::new().unwrap();
        let _guard = StateDirGuard::set(tmp.path());
        std::fs::write(port_path(tmp.path()), b"19999\n").unwrap();
        assert_eq!(probe_local_relay_port().await, None);
    }

    // The accept path needs a recorded pid the OS query would match, which can't be
    // staged in-process without leaking the discovery-trust seam into the discovery
    // tests; `security_tests/loopback_discovery_trust.rs` covers it.
    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn probe_local_relay_port_none_when_responder_is_not_the_recorded_daemon() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"instance_id": "x"})),
            )
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let _guard = StateDirGuard::set(tmp.path());
        let port = server.address().port();
        std::fs::write(port_path(tmp.path()), format!("{port}\n")).unwrap();

        assert_eq!(probe_local_relay_port().await, None);
    }

    // Pins the deliberately weak substring rule so a stronger check starts from a
    // known baseline. `tasklist` output is case-folded, `ps` argv is not.

    #[test]
    fn listing_names_a_real_server_process() {
        assert!(listing_names_server(
            "/usr/local/bin/inkentry-server --port 4655"
        ));
    }

    // A daemon still running from a pre-rename install is not recognised as ours.
    #[test]
    fn listing_does_not_name_a_pre_rename_server() {
        assert!(!listing_names_server(
            "/usr/local/bin/spelunk-server --port 4655"
        ));
    }

    // Any process whose argv merely contains the string passes.
    #[test]
    fn listing_names_an_unrelated_process_carrying_the_string() {
        assert!(listing_names_server(
            "python3 -c import time; time.sleep(8) inkentry-server"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn listing_names_server_case_insensitively_on_windows() {
        assert!(listing_names_server(
            "\"INKENTRY-SERVER.EXE\",\"4711\",\"Console\",\"1\",\"12,345 K\""
        ));
    }

    #[cfg(unix)]
    #[test]
    fn listing_is_case_sensitive_on_unix() {
        assert!(!listing_names_server("/usr/local/bin/INKENTRY-SERVER"));
    }

    #[tokio::test]
    async fn classify_foreign_when_no_port_and_no_match() {
        let tmp = TempDir::new().unwrap();
        let class = classify_running_server(tmp.path(), 999_999).await;
        assert!(
            matches!(class, RunningServer::Foreign),
            "expected Foreign when nothing identifies the PID as our server"
        );
    }

    #[tokio::test]
    async fn classify_foreign_when_unhealthy_and_no_match() {
        let tmp = TempDir::new().unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        std::fs::write(port_path(tmp.path()), format!("{port}\n")).unwrap();

        let class = classify_running_server(tmp.path(), 999_999).await;
        assert!(
            matches!(class, RunningServer::Foreign),
            "expected Foreign when /v1/health is silent and the PID isn't inkentry-server"
        );
    }

    #[tokio::test]
    async fn classify_healthy_when_health_responds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "instance_id": "abc123" })),
            )
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let port = server.address().port();
        std::fs::write(port_path(tmp.path()), format!("{port}\n")).unwrap();

        let class = classify_running_server(tmp.path(), 999_999).await;
        assert!(
            matches!(class, RunningServer::Healthy { .. }),
            "expected Healthy when /v1/health responds on the recorded port"
        );
    }

    #[test]
    fn read_db_path_round_trips() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(db_path_file(tmp.path()), "/some/where/server.db\n").unwrap();
        assert_eq!(
            read_db_path(tmp.path()),
            Some(PathBuf::from("/some/where/server.db"))
        );
    }

    #[test]
    fn read_db_path_none_when_missing_or_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(read_db_path(tmp.path()).is_none());
        std::fs::write(db_path_file(tmp.path()), "\n").unwrap();
        assert!(read_db_path(tmp.path()).is_none());
    }

    #[test]
    fn cleanup_removes_db_path_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(pid_path(tmp.path()), "1\n").unwrap();
        std::fs::write(port_path(tmp.path()), "4655\n").unwrap();
        std::fs::write(db_path_file(tmp.path()), "/x/server.db\n").unwrap();
        cleanup_state_files(tmp.path());
        assert!(!db_path_file(tmp.path()).exists());
        assert!(!pid_path(tmp.path()).exists());
        assert!(!port_path(tmp.path()).exists());
    }

    // A `fork()` in an unrelated test can transiently duplicate the fd table and
    // delay `flock` release by milliseconds; a short retry avoids crate-wide
    // serialisation.
    #[cfg(unix)]
    fn retry_acquire_start_lock(state_dir: &Path, timeout: Duration) -> Result<StartLock> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match acquire_start_lock(state_dir) {
                Ok(lock) => return Ok(lock),
                Err(e) if std::time::Instant::now() >= deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    // Asserts on `flock` release timing, which a concurrent fork+exec can delay
    // (the child inherits the lock fd until it execs); see `retry_acquire_start_lock`.
    #[cfg(unix)]
    #[test]
    #[serial(server_start_lock)]
    fn start_lock_is_exclusive_while_held() {
        let tmp = TempDir::new().unwrap();
        let first = acquire_start_lock(tmp.path()).expect("first lock acquires");
        assert!(
            acquire_start_lock(tmp.path()).is_err(),
            "second lock must fail while the first is held"
        );
        drop(first);
        assert!(
            retry_acquire_start_lock(tmp.path(), Duration::from_millis(500)).is_ok(),
            "lock frees on drop"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial(server_start_lock)]
    fn retry_acquire_start_lock_fails_when_lock_never_frees() {
        let tmp = TempDir::new().unwrap();
        let _held = acquire_start_lock(tmp.path()).expect("first lock acquires");
        assert!(
            retry_acquire_start_lock(tmp.path(), Duration::from_millis(50)).is_err(),
            "must fail when the lock genuinely never frees within the timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_state_dir_sets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("state");
        create_state_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "state dir should be 0700, got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn write_state_file_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("server.pid");
        write_state_file(&file, "12345\n").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file should be 0600, got {mode:o}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "12345\n");
    }

    #[cfg(unix)]
    #[test]
    fn open_log_file_for_append_sets_0600() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("server.log");
        {
            let mut f = open_log_file_for_append(&file).unwrap();
            f.write_all(b"line one\n").unwrap();
        }
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "log file should be 0600, got {mode:o}");
        {
            let mut f = open_log_file_for_append(&file).unwrap();
            f.write_all(b"line two\n").unwrap();
        }
        let contents = std::fs::read_to_string(&file).unwrap();
        assert_eq!(contents, "line one\nline two\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_state_file_refuses_to_follow_symlink() {
        let tmp = TempDir::new().unwrap();
        let outside_target = tmp.path().join("outside.txt");
        std::fs::write(&outside_target, "do not overwrite me").unwrap();

        let link_path = tmp.path().join("server.pid");
        std::os::unix::fs::symlink(&outside_target, &link_path).unwrap();

        let result = write_state_file(&link_path, "12345\n");
        assert!(
            result.is_err(),
            "write_state_file must refuse to follow a symlink at the target path"
        );
        assert_eq!(
            std::fs::read_to_string(&outside_target).unwrap(),
            "do not overwrite me"
        );
    }

    // `cmd_start`'s different-DB refusal needs a live daemon to exercise in full,
    // so the predicate behind it is covered directly.

    #[test]
    fn same_path_true_for_identical() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("server.db");
        std::fs::write(&p, b"x").unwrap();
        assert!(same_path(&p, &p));
    }

    #[test]
    fn same_path_false_for_distinct() {
        let tmp = TempDir::new().unwrap();
        assert!(!same_path(
            &tmp.path().join("a.db"),
            &tmp.path().join("b.db")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn same_path_true_across_symlink() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("real.db");
        std::fs::write(&target, b"x").unwrap();
        let link = tmp.path().join("link.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            same_path(&target, &link),
            "a symlink and its target are the same DB"
        );
    }

    #[tokio::test]
    async fn ensure_port_available_for_start_ok_when_free() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(ensure_port_available_for_start(port).await.is_ok());
    }

    #[tokio::test]
    async fn ensure_port_available_for_start_fails_when_port_held() {
        let _held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = _held.local_addr().unwrap().port();
        let err = ensure_port_available_for_start(port)
            .await
            .expect_err("must fail while the port is held");
        assert!(
            err.to_string().contains(&format!("port {port}")),
            "error should name the occupied port, got: {err}"
        );
    }

    // A background thread `wait()`s each child so a killed process can't linger as
    // a zombie, which still answers `kill(pid, 0)` and would fool `pid_is_alive`.

    #[cfg(unix)]
    struct DummyProc {
        pid: u32,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reaper: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(unix)]
    impl DummyProc {
        fn spawn(cmd: &mut std::process::Command) -> Self {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};

            let mut child = cmd
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn dummy process");
            let pid = child.id();
            let done = Arc::new(AtomicBool::new(false));
            let done_reaper = Arc::clone(&done);
            let reaper = std::thread::spawn(move || {
                let _ = child.wait();
                done_reaper.store(true, Ordering::SeqCst);
            });
            DummyProc {
                pid,
                done,
                reaper: Some(reaper),
            }
        }

        fn graceful() -> Self {
            DummyProc::spawn(std::process::Command::new("sleep").arg("30"))
        }

        // `SIG_IGN` set in `pre_exec` survives the `exec` into `sleep` (POSIX), so
        // there is no trap-install race and no shell child to orphan on SIGKILL.
        fn ignores_sigterm() -> Self {
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("30");
            // SAFETY: the closure only calls `signal`, which is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    unsafe extern "C" {
                        fn signal(signum: i32, handler: usize) -> usize;
                    }
                    const SIGTERM: i32 = 15;
                    const SIG_IGN: usize = 1;
                    signal(SIGTERM, SIG_IGN);
                    Ok(())
                });
            }
            DummyProc::spawn(&mut cmd)
        }

        fn named_server() -> (Self, TempDir) {
            use std::os::unix::fs::PermissionsExt;
            let dir = TempDir::new().unwrap();
            let bin = dir.path().join("inkentry-server");
            std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            (DummyProc::spawn(&mut std::process::Command::new(&bin)), dir)
        }
    }

    #[cfg(unix)]
    impl Drop for DummyProc {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;
            // Skipped once exited, to avoid signalling a reused PID.
            if !self.done.load(Ordering::SeqCst) {
                let _ = force_kill(self.pid);
            }
            if let Some(h) = self.reaper.take() {
                let _ = h.join();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial(server_start_lock)]
    fn process_matches_server_true_for_named_process() {
        let (proc, _dir) = DummyProc::named_server();
        assert!(
            process_matches_server(proc.pid),
            "a process whose command line contains 'inkentry-server' must match"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial(server_start_lock, path_env)]
    fn process_matches_server_false_for_unrelated_process() {
        let proc = DummyProc::graceful();
        assert!(
            !process_matches_server(proc.pid),
            "an unrelated process must not be mistaken for a inkentry-server"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock)]
    async fn classify_hung_ours_when_process_matches_but_health_silent() {
        let (proc, _dir) = DummyProc::named_server();
        let tmp = TempDir::new().unwrap(); // no server.port → health probe skipped
        let class = classify_running_server(tmp.path(), proc.pid).await;
        assert!(
            matches!(class, RunningServer::HungOurs),
            "expected HungOurs for a live inkentry-server process with silent health"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock, path_env)]
    async fn wait_for_exit_false_for_live_process() {
        let proc = DummyProc::graceful();
        assert!(
            !wait_for_exit(proc.pid, Duration::from_millis(300)).await,
            "a live process must not be reported as exited"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock, path_env)]
    async fn terminate_and_wait_stops_graceful_process() {
        let proc = DummyProc::graceful();
        assert!(pid_is_alive(proc.pid));
        let stopped = terminate_and_wait(proc.pid).await.expect("terminate");
        assert!(stopped, "graceful process should be reported stopped");
        assert!(!pid_is_alive(proc.pid), "process must actually be gone");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock, path_env)]
    async fn force_kill_reaps_sigterm_ignoring_process() {
        let proc = DummyProc::ignores_sigterm();
        assert!(pid_is_alive(proc.pid));
        terminate_process(proc.pid).expect("SIGTERM");
        assert!(
            !wait_for_exit(proc.pid, Duration::from_millis(400)).await,
            "SIGTERM-ignoring process should survive SIGTERM"
        );
        force_kill(proc.pid).expect("SIGKILL");
        assert!(
            wait_for_exit(proc.pid, FORCE_KILL_TIMEOUT).await,
            "SIGKILL must reap the process"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial(server_start_lock, path_env)]
    async fn terminate_and_wait_escalates_when_sigterm_ignored() {
        let proc = DummyProc::ignores_sigterm();
        assert!(pid_is_alive(proc.pid));
        let stopped = terminate_and_wait(proc.pid)
            .await
            .expect("terminate should not error");
        assert!(
            stopped,
            "a SIGTERM-ignoring daemon must still be stopped via SIGKILL"
        );
        assert!(
            !pid_is_alive(proc.pid),
            "success must mean the PID is actually gone"
        );
    }
}
