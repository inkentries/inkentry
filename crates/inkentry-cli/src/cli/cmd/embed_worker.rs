// State files are keyed by a hash of the index path: workers are per-project,
// the state dir is per-machine. `embed-worker-<key>.baseline` holds
// `<started_at_unix> <pending_tokens>` at worker start. The ETA rate is measured
// per run and never persisted, because the token estimate's bias is
// corpus-dependent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::server::{create_state_dir, pid_is_alive, write_state_file};
use crate::capability::inkentry_state_dir;
use crate::storage::Database;

fn worker_key(db_path: &Path) -> String {
    let canonical = inkentry_core::utils::canonicalize(db_path);
    let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
    hash.to_hex()[..16].to_string()
}

fn pid_file(state_dir: &Path, key: &str) -> PathBuf {
    state_dir.join(format!("embed-worker-{key}.pid"))
}

fn baseline_file(state_dir: &Path, key: &str) -> PathBuf {
    state_dir.join(format!("embed-worker-{key}.baseline"))
}

// Best-effort: state-file failures must not fail the embed.
pub(super) struct EmbedWorkerGuard {
    pid_path: PathBuf,
    baseline_path: PathBuf,
}

impl EmbedWorkerGuard {
    pub(super) fn acquire(db: &Database, db_path: &Path) -> Option<Self> {
        let state_dir = inkentry_state_dir().ok()?;
        create_state_dir(&state_dir).ok()?;
        let key = worker_key(db_path);
        let pid_path = pid_file(&state_dir, &key);
        let baseline_path = baseline_file(&state_dir, &key);

        write_state_file(&pid_path, &format!("{}\n", std::process::id())).ok()?;

        let pending = db
            .embed_token_stats()
            .map(|s| s.pending_tokens)
            .unwrap_or(0);
        let now = chrono::Utc::now().timestamp();
        let _ = write_state_file(&baseline_path, &format!("{now} {pending}\n"));

        Some(Self {
            pid_path,
            baseline_path,
        })
    }
}

impl Drop for EmbedWorkerGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_path);
        let _ = std::fs::remove_file(&self.baseline_path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkerLiveness {
    Alive,
    NotRunning,
}

// A pid can be recycled by an unrelated process after a crash, so liveness alone
// must not read as a running embed.
fn classify_worker_pid(alive: bool, looks_like_worker: bool) -> WorkerLiveness {
    if alive && looks_like_worker {
        WorkerLiveness::Alive
    } else {
        WorkerLiveness::NotRunning
    }
}

// Exact match, never substring: a checkout path or unrelated binary can contain
// "inkentry".
fn is_inkentry_exe_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("inkentry") || name.eq_ignore_ascii_case("inkentry.exe")
}

// Matches parsed argv0 and tokens rather than a substring over the line, which a
// path like `.../inkentry/index-workspace/...` would satisfy.
#[cfg(any(unix, test))]
fn command_looks_like_index_run(command_line: &str) -> bool {
    let mut tokens = command_line.split_whitespace();
    let Some(argv0) = tokens.next() else {
        return false;
    };
    let exe_name = Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(argv0);
    is_inkentry_exe_name(exe_name) && tokens.any(|t| t == "index")
}

// `tasklist` exposes no argv, so exact image-name equality is the strongest
// identity check available.
#[cfg(any(windows, test))]
fn tasklist_line_matches_inkentry(line: &str) -> bool {
    let image_name = line
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    is_inkentry_exe_name(image_name)
}

fn process_looks_like_index_run(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "args="])
            .output()
        {
            Ok(out) if out.status.success() => {
                command_looks_like_index_run(&String::from_utf8_lossy(&out.stdout))
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
                tasklist_line_matches_inkentry(&String::from_utf8_lossy(&out.stdout))
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

pub(super) fn worker_liveness(db_path: &Path) -> WorkerLiveness {
    let Ok(state_dir) = inkentry_state_dir() else {
        return WorkerLiveness::NotRunning;
    };
    let key = worker_key(db_path);
    let pid_path = pid_file(&state_dir, &key);
    let Some(pid) = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return WorkerLiveness::NotRunning;
    };
    match classify_worker_pid(pid_is_alive(pid), process_looks_like_index_run(pid)) {
        WorkerLiveness::Alive => WorkerLiveness::Alive,
        WorkerLiveness::NotRunning => {
            let _ = std::fs::remove_file(&pid_path);
            let _ = std::fs::remove_file(baseline_file(&state_dir, &key));
            WorkerLiveness::NotRunning
        }
    }
}

pub(super) fn worker_eta(db_path: &Path, pending_tokens_now: i64) -> Option<Duration> {
    let state_dir = inkentry_state_dir().ok()?;
    let key = worker_key(db_path);
    let contents = std::fs::read_to_string(baseline_file(&state_dir, &key)).ok()?;
    let mut parts = contents.split_whitespace();
    let started_at: i64 = parts.next()?.parse().ok()?;
    let pending_at_start: i64 = parts.next()?.parse().ok()?;
    eta_from_baseline(
        started_at,
        pending_at_start,
        chrono::Utc::now().timestamp(),
        pending_tokens_now,
    )
}

fn eta_from_baseline(
    started_at: i64,
    pending_at_start: i64,
    now: i64,
    pending_now: i64,
) -> Option<Duration> {
    let elapsed = now.checked_sub(started_at)?;
    let drained = pending_at_start.checked_sub(pending_now)?;
    if elapsed <= 0 || drained <= 0 || pending_now <= 0 {
        return None;
    }
    let rate = drained as f64 / elapsed as f64; // tokens per second
    let secs = pending_now as f64 / rate;
    if !secs.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alive_and_matching_command_is_a_live_worker() {
        assert_eq!(classify_worker_pid(true, true), WorkerLiveness::Alive);
    }

    #[test]
    fn dead_pid_is_not_running() {
        assert_eq!(classify_worker_pid(false, true), WorkerLiveness::NotRunning);
        assert_eq!(
            classify_worker_pid(false, false),
            WorkerLiveness::NotRunning
        );
    }

    #[test]
    fn foreign_pid_is_never_reported_as_a_live_worker() {
        assert_eq!(classify_worker_pid(true, false), WorkerLiveness::NotRunning);
    }

    #[test]
    fn detached_embed_worker_command_line_matches() {
        assert!(command_looks_like_index_run(
            "/home/user/inkentry/target/release/inkentry index /home/user/proj --_embed-phases --batch-size 8"
        ));
    }

    #[test]
    fn foreground_resume_command_line_matches() {
        assert!(command_looks_like_index_run(
            "/usr/local/bin/inkentry index /home/user/proj"
        ));
    }

    #[test]
    fn path_containing_both_substrings_but_wrong_binary_does_not_match() {
        assert!(!command_looks_like_index_run(
            "/home/user/inkentry/index-workspace/target/debug/deps/e2e_cli-abc123 some_test_name"
        ));
    }

    #[test]
    fn inkentry_binary_without_an_index_token_does_not_match() {
        assert!(!command_looks_like_index_run(
            "/usr/local/bin/inkentry status"
        ));
    }

    #[test]
    fn a_token_merely_containing_index_is_not_the_exact_subcommand() {
        assert!(!command_looks_like_index_run(
            "/usr/local/bin/inkentry reindex-everything"
        ));
    }

    #[test]
    fn empty_command_line_does_not_match() {
        assert!(!command_looks_like_index_run(""));
        assert!(!command_looks_like_index_run("   "));
    }

    #[test]
    fn exact_binary_names_match_case_insensitively() {
        assert!(is_inkentry_exe_name("inkentry"));
        assert!(is_inkentry_exe_name("INKENTRY"));
        assert!(is_inkentry_exe_name("inkentry.exe"));
        assert!(is_inkentry_exe_name("Inkentry.EXE"));
    }

    #[test]
    fn names_only_containing_inkentry_as_a_substring_do_not_match() {
        assert!(!is_inkentry_exe_name("notinkentry"));
        assert!(!is_inkentry_exe_name("inkentry-tool"));
        assert!(!is_inkentry_exe_name("my-inkentry-thing.exe"));
    }

    #[test]
    fn tasklist_csv_line_with_exact_image_name_matches() {
        assert!(tasklist_line_matches_inkentry(
            r#""inkentry.exe","1234","Console","1","12,345 K""#
        ));
    }

    #[test]
    fn tasklist_csv_line_with_substring_only_image_name_does_not_match() {
        assert!(!tasklist_line_matches_inkentry(
            r#""notinkentry.exe","1234","Console","1","12,345 K""#
        ));
    }

    #[test]
    fn worker_key_differs_per_project() {
        let a = worker_key(Path::new("/proj-a/.inkentry/index.db"));
        let b = worker_key(Path::new("/proj-b/.inkentry/index.db"));
        assert_ne!(a, b, "two projects must never share a liveness file");
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn worker_key_is_stable_for_the_same_path() {
        let p = Path::new("/proj-a/.inkentry/index.db");
        assert_eq!(worker_key(p), worker_key(p));
    }

    #[test]
    fn eta_scales_with_pending_tokens_at_the_measured_rate() {
        let eta = eta_from_baseline(0, 6000, 100, 5000).expect("measurable progress");
        assert_eq!(eta, Duration::from_secs(500));
    }

    #[test]
    fn eta_is_none_while_calibrating() {
        assert!(eta_from_baseline(0, 6000, 100, 6000).is_none());
        assert!(eta_from_baseline(100, 6000, 100, 5000).is_none());
        assert!(eta_from_baseline(0, 6000, 100, 0).is_none());
    }

    #[test]
    fn eta_tolerates_a_clock_step_or_regressed_baseline() {
        assert!(eta_from_baseline(200, 6000, 100, 5000).is_none());
        assert!(eta_from_baseline(0, 5000, 100, 6000).is_none());
    }

    #[test]
    fn eta_survives_extreme_token_counts_without_panicking() {
        let eta = eta_from_baseline(0, i64::MAX, 1, i64::MAX - 1000);
        assert!(eta.is_some(), "a huge but finite ETA is still an ETA");
        assert!(eta_from_baseline(0, i64::MIN, 1, i64::MAX).is_none());
    }
}
