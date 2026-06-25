//! `spelunk server` subcommand — manage a local spelunk-server daemon.
//!
//! ## Subcommands
//!
//! - `spelunk server start`  — daemonise spelunk-server; write PID/port/log files.
//! - `spelunk server stop`   — send SIGTERM to the running daemon and wait for exit.
//! - `spelunk server status` — print PID, port, instance_id, and uptime.
//! - `spelunk server logs`   — print the last N lines from the server log.
//!
//! ## State directory
//!
//! All runtime state lives under `~/.local/state/spelunk/`:
//! - `server.pid`  — PID of the running daemon process
//! - `server.port` — TCP port the daemon is listening on (read by `capability.rs`)
//! - `server.log`  — stdout + stderr of the daemon process
//!
//! The port file is read by `capability.rs` for loopback auto-discovery
//! (spelunk#316).  The writer here **must** use the same path.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

// ── State dir helpers ─────────────────────────────────────────────────────────

/// `~/.local/state/spelunk/` on all platforms.
///
/// Mirrors `spelunk_state_dir()` in `capability.rs` — both reader and writer
/// must use the same path.
pub fn spelunk_state_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".local").join("state").join("spelunk"))
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
}

fn pid_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.pid")
}
fn port_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.port")
}
fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("server.log")
}

/// Read PID from the state file. Returns `None` if absent or unparseable.
fn read_pid(state_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path(state_dir))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Read port from the state file. Returns `None` if absent or unparseable.
fn read_port(state_dir: &Path) -> Option<u16> {
    std::fs::read_to_string(port_path(state_dir))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

/// Return `true` when `pid` names a currently-running process.
fn pid_is_alive(pid: u32) -> bool {
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
        // OpenProcess with PROCESS_QUERY_LIMITED_INFORMATION is sufficient to
        // call GetExitCodeProcess.  A NULL handle means the process does not
        // exist (or we have no access — treated as "not alive").
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
        // Unknown platform: conservatively return false so stale PIDs do not
        // block a fresh server start.
        let _ = pid;
        false
    }
}

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: ServerCommand,
}

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Start a local spelunk-server daemon (idempotent)
    Start(ServerStartArgs),
    /// Stop the running local spelunk-server daemon
    Stop,
    /// Show status of the local spelunk-server daemon
    Status,
    /// Print the last N lines of the server log
    Logs(ServerLogsArgs),
}

#[derive(Args, Debug)]
pub struct ServerStartArgs {
    /// Port to try first (then 7778–7787 on collision)
    #[arg(long, default_value = "7777")]
    pub port: u16,

    /// Path to the spelunk-server binary (default: the `spelunk-server` in PATH)
    #[arg(long)]
    pub bin: Option<PathBuf>,

    /// Path to the server SQLite database (default: ~/.local/state/spelunk/server.db)
    #[arg(long)]
    pub db: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ServerLogsArgs {
    /// Number of lines to show (default: 50)
    #[arg(short = 'n', long, default_value = "50")]
    pub lines: usize,
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

pub async fn server(args: ServerArgs) -> Result<()> {
    match args.command {
        ServerCommand::Start(a) => cmd_start(a).await,
        ServerCommand::Stop => cmd_stop().await,
        ServerCommand::Status => cmd_status().await,
        ServerCommand::Logs(a) => cmd_logs(a),
    }
}

// ── Public bootstrap API ──────────────────────────────────────────────────────

/// Ensure a local spelunk-server is running.
///
/// Returns `(port, freshly_started)`. Idempotent: if the server is already
/// healthy, returns immediately with `freshly_started = false`.
///
/// Called by `spelunk init` to auto-spawn the server when running interactively.
pub async fn ensure_server_running(start_port: u16) -> Result<(u16, bool)> {
    let state_dir = spelunk_state_dir()?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;

    // Already running and healthy?
    if let Some(pid) = read_pid(&state_dir)
        && pid_is_alive(pid)
        && let Some(port) = read_port(&state_dir)
        && probe_health(port).await.is_some()
    {
        return Ok((port, false));
    }

    let bin = which_spelunk_server()?;
    let db = state_dir.join("server.db");
    const PORT_RANGE: u16 = 11;
    let port = find_available_port(start_port, PORT_RANGE)?;

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(&state_dir))
        .with_context(|| format!("opening {}", log_path(&state_dir).display()))?;

    #[cfg(unix)]
    let child = spawn_daemon_unix(&bin, &db, port, log_file)?;
    #[cfg(windows)]
    let child = spawn_daemon_windows(&bin, &db, port, log_file)?;

    let pid = child.id();
    std::fs::write(pid_path(&state_dir), format!("{pid}\n")).context("writing server.pid")?;
    std::fs::write(port_path(&state_dir), format!("{port}\n")).context("writing server.port")?;

    let ready = wait_for_health(port, Duration::from_secs(5)).await;
    if !ready {
        tracing::warn!("spelunk-server started (pid={pid}) but did not respond within 5 s");
    }

    Ok((port, true))
}

// ── start ─────────────────────────────────────────────────────────────────────

async fn cmd_start(args: ServerStartArgs) -> Result<()> {
    let state_dir = spelunk_state_dir()?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;

    // ── Idempotency check ────────────────────────────────────────────────────
    if let Some(pid) = read_pid(&state_dir)
        && pid_is_alive(pid)
    {
        if let Some(port) = read_port(&state_dir)
            && probe_health(port).await.is_some()
        {
            println!("spelunk-server is already running (pid={pid}, port={port}).");
            return Ok(());
        }
        // PID alive but no health response — stale state; fall through to restart.
        tracing::warn!("pid {pid} is alive but /v1/health did not respond; restarting");
    }

    // ── Find the binary ──────────────────────────────────────────────────────
    let bin = match &args.bin {
        Some(p) => {
            if !p.exists() {
                anyhow::bail!("spelunk-server binary not found at {}", p.display());
            }
            p.clone()
        }
        None => which_spelunk_server()?,
    };

    // ── Default DB path ──────────────────────────────────────────────────────
    let db = args.db.unwrap_or_else(|| state_dir.join("server.db"));

    // ── Port selection (7777–7787) ───────────────────────────────────────────
    const PORT_RANGE: u16 = 11; // 7777..=7787
    let port = find_available_port(args.port, PORT_RANGE)?;

    // ── Spawn daemonised process ─────────────────────────────────────────────
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(&state_dir))
        .with_context(|| format!("opening {}", log_path(&state_dir).display()))?;

    #[cfg(unix)]
    let child = spawn_daemon_unix(&bin, &db, port, log_file)?;
    #[cfg(windows)]
    let child = spawn_daemon_windows(&bin, &db, port, log_file)?;

    let pid = child.id();

    // Write state files.
    std::fs::write(pid_path(&state_dir), format!("{pid}\n")).context("writing server.pid")?;
    std::fs::write(port_path(&state_dir), format!("{port}\n")).context("writing server.port")?;

    // Wait up to 5 s for the server to become reachable.
    let ready = wait_for_health(port, Duration::from_secs(5)).await;
    if ready {
        println!("spelunk-server started (pid={pid}, port={port}).");
        println!("  Log: {}", log_path(&state_dir).display());
    } else {
        eprintln!(
            "warning: spelunk-server process started (pid={pid}) but did not respond \
             on port {port} within 5 s. Check the log: {}",
            log_path(&state_dir).display()
        );
    }

    Ok(())
}

/// Locate the `spelunk-server` binary.
///
/// Priority: next to the current executable → PATH.
fn which_spelunk_server() -> Result<PathBuf> {
    // On Windows executables carry a `.exe` suffix; on Unix there is no suffix.
    #[cfg(windows)]
    let bin_name = "spelunk-server.exe";
    #[cfg(not(windows))]
    let bin_name = "spelunk-server";

    // 1. Same directory as the running `spelunk` binary.
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(bin_name);
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    // 2. PATH lookup.
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(bin_name))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "spelunk-server binary not found. \
                 Install it alongside `spelunk` or pass --bin <path>."
            )
        })
}

/// Walk ports `start..start+range` to find the first unbound one.
fn find_available_port(start: u16, range: u16) -> Result<u16> {
    for offset in 0..range {
        let port = start.saturating_add(offset);
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "No free port found in {}–{}.  Stop another service or pass --port.",
        start,
        start.saturating_add(range - 1),
    )
}

/// Build the argument list passed to `spelunk-server` when auto-spawning the daemon.
///
/// Extracted from the spawn helpers so that unit tests can verify the args
/// without actually launching a process (THREAT-MODEL req #9 / decision #88).
///
/// The returned `Vec` contains every argument **after** the binary path, in
/// order, as it would be appended to `std::process::Command`.
pub(super) fn build_daemon_args(db: &Path, port: u16) -> Vec<std::ffi::OsString> {
    vec![
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
        "--db".into(),
        db.as_os_str().into(),
    ]
}

/// Spawn the server on Unix.
///
/// Uses a single `fork`+`exec` via `std::process::Command::spawn()`.  The
/// child process inherits the log file handles and runs independently; the
/// CLI process exits after writing the PID/port state files, at which point
/// the child is reparented to init/launchd and becomes fully detached.
///
/// `--host 127.0.0.1` is always passed so the auto-spawned daemon only binds
/// the loopback interface (THREAT-MODEL req #9 / decision #88).  Without this
/// flag spelunk-server defaults to 0.0.0.0 and would be LAN-reachable while
/// unauthenticated.
#[cfg(unix)]
fn spawn_daemon_unix(
    bin: &Path,
    db: &Path,
    port: u16,
    log_file: std::fs::File,
) -> Result<std::process::Child> {
    let log_file_err = log_file.try_clone().context("cloning log file handle")?;

    let mut cmd = std::process::Command::new(bin);
    for arg in build_daemon_args(db, port) {
        cmd.arg(arg);
    }
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_file_err)
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;

    Ok(child)
}

/// Spawn the server on Windows with `CREATE_NEW_PROCESS_GROUP`.
///
/// `--host 127.0.0.1` is always passed so the auto-spawned daemon only binds
/// the loopback interface (THREAT-MODEL req #9 / decision #88).  Without this
/// flag spelunk-server defaults to 0.0.0.0 and would be LAN-reachable while
/// unauthenticated.
#[cfg(windows)]
fn spawn_daemon_windows(
    bin: &Path,
    db: &Path,
    port: u16,
    log_file: std::fs::File,
) -> Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let mut cmd = std::process::Command::new(bin);
    for arg in build_daemon_args(db, port) {
        cmd.arg(arg);
    }
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;

    Ok(child)
}

/// Poll `GET http://127.0.0.1:{port}/v1/health` until it responds or timeout.
async fn wait_for_health(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if probe_health(port).await.is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Single non-retrying health probe. Returns the `instance_id` on success.
async fn probe_health(port: u16) -> Option<String> {
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
    body.instance_id.or_else(|| Some("unknown".into()))
}

// ── stop ──────────────────────────────────────────────────────────────────────

async fn cmd_stop() -> Result<()> {
    let state_dir = spelunk_state_dir()?;
    let pid = read_pid(&state_dir)
        .ok_or_else(|| anyhow::anyhow!("no server.pid found — is spelunk-server running?"))?;

    if !pid_is_alive(pid) {
        println!("spelunk-server (pid={pid}) is not running. Cleaning up state files.");
        cleanup_state_files(&state_dir);
        return Ok(());
    }

    // Send SIGTERM (Unix) or TerminateProcess (Windows).
    terminate_process(pid)?;

    // Wait up to 10 s for the process to exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !pid_is_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    if pid_is_alive(pid) {
        eprintln!("warning: spelunk-server (pid={pid}) did not stop within 10 s.");
    } else {
        println!("spelunk-server stopped.");
        cleanup_state_files(&state_dir);
    }

    Ok(())
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
        // On Windows, use taskkill.
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

fn cleanup_state_files(state_dir: &Path) {
    let _ = std::fs::remove_file(pid_path(state_dir));
    let _ = std::fs::remove_file(port_path(state_dir));
}

// ── status ────────────────────────────────────────────────────────────────────

async fn cmd_status() -> Result<()> {
    let state_dir = spelunk_state_dir()?;
    let pid = read_pid(&state_dir);
    let port = read_port(&state_dir);

    match (pid, port) {
        (Some(pid), Some(port)) if pid_is_alive(pid) => {
            println!("spelunk-server  \x1b[32mrunning\x1b[0m");
            println!("  PID:   {pid}");
            println!("  Port:  {port}");
            println!("  Log:   {}", log_path(&state_dir).display());

            // Fetch extended info from /v1/health.
            match probe_health_verbose(port).await {
                Some(info) => {
                    println!("  URL:   http://127.0.0.1:{port}");
                    if let Some(id) = info.instance_id {
                        println!("  ID:    {id}");
                    }
                    if let Some(ver) = info.version {
                        println!("  Ver:   {ver}");
                    }
                }
                None => {
                    println!("  URL:   http://127.0.0.1:{port}  \x1b[31m(unreachable)\x1b[0m");
                }
            }
        }
        (Some(pid), _) if pid_is_alive(pid) => {
            println!("spelunk-server  \x1b[33mrunning\x1b[0m (port unknown)");
            println!("  PID: {pid}");
        }
        (Some(pid), _) => {
            println!("spelunk-server  \x1b[31mstopped\x1b[0m (stale pid={pid})");
            println!("  Run `spelunk server start` to start.");
        }
        (None, _) => {
            println!("spelunk-server  \x1b[31mnot started\x1b[0m");
            println!("  Run `spelunk server start` to start.");
        }
    }
    Ok(())
}

struct HealthInfo {
    instance_id: Option<String>,
    version: Option<String>,
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
    struct H {
        instance_id: Option<String>,
        version: Option<String>,
    }
    let body: H = resp.json().await.ok()?;
    Some(HealthInfo {
        instance_id: body.instance_id,
        version: body.version,
    })
}

// ── logs ──────────────────────────────────────────────────────────────────────

fn cmd_logs(args: ServerLogsArgs) -> Result<()> {
    let state_dir = spelunk_state_dir()?;
    let log = log_path(&state_dir);

    if !log.exists() {
        anyhow::bail!(
            "No log file at {}. Start the server first with `spelunk server start`.",
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── spelunk_state_dir ────────────────────────────────────────────────────

    #[test]
    fn state_dir_contains_spelunk() {
        let dir = spelunk_state_dir().expect("state dir");
        assert!(
            dir.to_string_lossy().contains("spelunk"),
            "state dir should contain 'spelunk', got {dir:?}"
        );
    }

    // ── find_available_port ──────────────────────────────────────────────────

    #[test]
    fn find_available_port_succeeds() {
        // Port 0 triggers OS assignment; we use a high ephemeral range that is
        // very likely free in CI.
        let port = find_available_port(19700, 20).expect("should find a free port");
        assert!((19700..19720).contains(&port));
    }

    #[test]
    fn find_available_port_fails_when_all_bound() {
        // Bind to every port in a tiny range, then verify we get an error.
        let range: u16 = 3;
        let start: u16 = 19750;
        let _listeners: Vec<std::net::TcpListener> = (start..start + range)
            .filter_map(|p| std::net::TcpListener::bind(("127.0.0.1", p)).ok())
            .collect();
        // Only error if we actually managed to bind all three.
        if _listeners.len() == range as usize {
            assert!(
                find_available_port(start, range).is_err(),
                "expected error when all ports are bound"
            );
        }
    }

    // ── pid_is_alive ─────────────────────────────────────────────────────────

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(pid_is_alive(pid), "current process should be alive");
    }

    // ── read_pid / read_port ─────────────────────────────────────────────────

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
        std::fs::write(port_path(tmp.path()), b"7777\n").unwrap();
        assert_eq!(read_port(tmp.path()), Some(7777));
    }

    // ── which_spelunk_server ─────────────────────────────────────────────────

    #[test]
    fn which_spelunk_server_finds_sibling_binary() {
        // Create a fake `spelunk-server[.exe]` next to the current executable.
        let tmp = TempDir::new().unwrap();
        // On Windows the binary must have the .exe extension to be recognised
        // as a file by the PATH search in `which_spelunk_server`.
        #[cfg(windows)]
        let fake_bin = tmp.path().join("spelunk-server.exe");
        #[cfg(not(windows))]
        let fake_bin = tmp.path().join("spelunk-server");
        std::fs::write(&fake_bin, b"").unwrap();

        // Temporarily redirect PATH so only our fake bin is discoverable and
        // pretend current_exe lives in tmp.
        //
        // We can't override current_exe() at runtime, so just verify the PATH
        // fallback path: put tmp on PATH and confirm discovery succeeds.
        //
        // SAFETY: test binary is single-threaded at this point; no other thread
        // reads PATH concurrently within this test.
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        // Use the platform PATH separator (`;` on Windows, `:` on Unix).
        #[cfg(windows)]
        let new_path = format!("{};{}", tmp.path().display(), old_path.to_string_lossy());
        #[cfg(not(windows))]
        let new_path = format!("{}:{}", tmp.path().display(), old_path.to_string_lossy());
        unsafe { std::env::set_var("PATH", &new_path) };
        let result = which_spelunk_server();
        unsafe { std::env::set_var("PATH", &old_path) };

        assert!(result.is_ok(), "should discover binary on PATH: {result:?}");
    }

    #[test]
    fn which_spelunk_server_fails_when_not_on_path() {
        // SAFETY: see note in which_spelunk_server_finds_sibling_binary.
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", "") };
        let result = which_spelunk_server();
        unsafe { std::env::set_var("PATH", &old_path) };
        assert!(result.is_err(), "should fail when binary is not on PATH");
    }

    // ── spawn_daemon arg list (THREAT-MODEL req #9) ──────────────────────────
    //
    // Security invariant: the auto-spawned spelunk-server daemon MUST bind
    // only the loopback interface.  Without `--host 127.0.0.1` the server
    // defaults to 0.0.0.0 and becomes LAN-reachable while unauthenticated.
    //
    // These tests verify the arg list produced by `build_daemon_args` — the
    // single source of truth for both the Unix and Windows spawn helpers —
    // so that a future refactor cannot silently drop the flag.
    //
    // refs: https://github.com/spelunk-cloud/spelunk/pull/365

    /// `--host 127.0.0.1` must appear in the daemon arg list (THREAT-MODEL req #9).
    #[test]
    fn spawn_daemon_args_bind_loopback() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let args = build_daemon_args(&db, 7777);

        // Collect as strings for readable assertions.
        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // `--host` flag must be present.
        assert!(
            args_str.contains(&"--host".to_string()),
            "THREAT-MODEL req #9: --host flag missing from daemon args: {args_str:?}"
        );

        // The value immediately following `--host` must be `127.0.0.1`.
        let host_idx = args_str
            .iter()
            .position(|a| a == "--host")
            .expect("--host must be present");
        let host_value = args_str
            .get(host_idx + 1)
            .expect("--host must be followed by a value");
        assert_eq!(
            host_value, "127.0.0.1",
            "THREAT-MODEL req #9: daemon must bind 127.0.0.1 only, got {host_value:?}"
        );
    }

    /// `0.0.0.0` must NOT appear in the daemon arg list (THREAT-MODEL req #9).
    #[test]
    fn spawn_daemon_args_do_not_bind_wildcard() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let args = build_daemon_args(&db, 7777);

        let args_str: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(
            !args_str.contains(&"0.0.0.0".to_string()),
            "THREAT-MODEL req #9: daemon args must not contain 0.0.0.0 (wildcard bind): {args_str:?}"
        );
    }

    /// `--port` and the supplied port value must appear in the daemon arg list.
    #[test]
    fn spawn_daemon_args_include_port() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let port: u16 = 7780;
        let args = build_daemon_args(&db, port);

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

    /// `--db` and the supplied path must appear in the daemon arg list.
    #[test]
    fn spawn_daemon_args_include_db_path() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("server.db");
        let args = build_daemon_args(&db, 7777);

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
}
