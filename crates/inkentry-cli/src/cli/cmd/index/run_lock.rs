// Two concurrent index runs corrupt index.db (SQLITE_CORRUPT); SQLite's own
// locking does not stop them interleaving writes across a multi-transaction
// run, so whole runs are serialized with an OS advisory lock.

use anyhow::Result;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

const LOCK_FILE_NAME: &str = "index.lock";
// Never locked: Windows `LockFileEx` denies reads of a locked file from a
// second handle, so the pid cannot live in the lock file itself.
const LOCK_PID_FILE_NAME: &str = "index.lock.pid";

// Dropping closes the fd and releases the lock, so a killed holder leaves no
// stale lock to clean up.
pub struct IndexRunLock {
    _file: File,
}

pub enum LockOutcome {
    Acquired(IndexRunLock),
    HeldByOther {
        // Best-effort and display-only; the OS lock is what excludes.
        holder_pid: Option<u32>,
    },
}

// Non-blocking: a run's write window is unbounded, so waiting would hang a
// second invocation instead of failing fast.
pub fn try_acquire(inkentry_dir: &Path) -> Result<LockOutcome> {
    std::fs::create_dir_all(inkentry_dir)?;
    let path = inkentry_dir.join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    match file.try_lock() {
        Ok(()) => {
            let pid_path = inkentry_dir.join(LOCK_PID_FILE_NAME);
            std::fs::write(&pid_path, std::process::id().to_string()).ok();
            Ok(LockOutcome::Acquired(IndexRunLock { _file: file }))
        }
        Err(TryLockError::WouldBlock) => {
            let pid_path = inkentry_dir.join(LOCK_PID_FILE_NAME);
            let holder_pid = std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|s| s.trim().parse().ok());
            Ok(LockOutcome::HeldByOther { holder_pid })
        }
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

fn read_recorded_pid(inkentry_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(inkentry_dir.join(LOCK_PID_FILE_NAME))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

// Confirms the continuation itself became the holder, not a third process that
// raced into the gap between the caller's drop and the continuation's acquire.
pub fn wait_for_holder_pid(
    inkentry_dir: &Path,
    expected_pid: u32,
    timeout: Duration,
    poll_interval: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if read_recorded_pid(inkentry_dir) == Some(expected_pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll_interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_on_the_same_dir_is_held_by_other() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_acquire(dir.path()).expect("first acquire");
        assert!(matches!(first, LockOutcome::Acquired(_)));

        let second = try_acquire(dir.path()).expect("second acquire attempt");
        assert!(
            matches!(second, LockOutcome::HeldByOther { .. }),
            "a live holder must make a concurrent acquire report contention, not succeed"
        );
    }

    #[test]
    fn holder_pid_is_recorded_for_the_error_message() {
        let dir = tempfile::tempdir().unwrap();
        let _first = try_acquire(dir.path()).expect("first acquire");

        let second = try_acquire(dir.path()).expect("second acquire attempt");
        match second {
            LockOutcome::HeldByOther { holder_pid } => {
                assert_eq!(
                    holder_pid,
                    Some(std::process::id()),
                    "the holder pid recorded in the lock file must be this test process's own \
                     pid (it holds the lock via `first`)"
                );
            }
            LockOutcome::Acquired(_) => panic!("must be held by other"),
        }
    }

    #[test]
    fn lock_is_released_when_the_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        {
            let first = try_acquire(dir.path()).expect("first acquire");
            assert!(matches!(first, LockOutcome::Acquired(_)));
        }

        let second = try_acquire(dir.path()).expect("second acquire attempt");
        assert!(
            matches!(second, LockOutcome::Acquired(_)),
            "once the first guard drops, a fresh acquire must succeed"
        );
    }

    #[test]
    fn wait_for_holder_pid_returns_true_once_content_already_matches() {
        let dir = tempfile::tempdir().unwrap();
        let _held = try_acquire(dir.path()).expect("acquire");
        assert!(wait_for_holder_pid(
            dir.path(),
            std::process::id(),
            Duration::from_millis(200),
            Duration::from_millis(5),
        ));
    }

    #[test]
    fn wait_for_holder_pid_times_out_when_the_recorded_pid_never_matches() {
        let dir = tempfile::tempdir().unwrap();
        let _held = try_acquire(dir.path()).expect("acquire");

        let started = Instant::now();
        let confirmed = wait_for_holder_pid(
            dir.path(),
            std::process::id().wrapping_add(1),
            Duration::from_millis(150),
            Duration::from_millis(10),
        );
        assert!(
            !confirmed,
            "must not confirm a pid that was never the recorded holder"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "must wait out the full timeout rather than returning early"
        );
    }

    #[test]
    fn wait_for_holder_pid_detects_a_holder_that_appears_after_a_delay() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let pid = std::process::id();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            let held = try_acquire(&dir_path).expect("delayed acquire");
            std::thread::sleep(Duration::from_millis(500));
            drop(held);
        });

        assert!(
            wait_for_holder_pid(
                dir.path(),
                pid,
                Duration::from_millis(500),
                Duration::from_millis(10),
            ),
            "must detect a holder that appears mid-poll, not just one already present at the \
             first check"
        );
    }
}
