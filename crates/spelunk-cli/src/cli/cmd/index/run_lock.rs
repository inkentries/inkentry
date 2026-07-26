//! Cross-process advisory lock serializing whole `spelunk index` runs against
//! one project's DB.
//!
//! Two `spelunk index` processes racing on the same project reproducibly
//! corrupt `index.db` (`SQLITE_CORRUPT`, not merely `SQLITE_BUSY`) - neither
//! `index.db` nor `memory.db` nor `registry.db` sets `PRAGMA busy_timeout`
//! anywhere in this codebase, and SQLite's own per-connection locking does
//! not prevent two independent processes from interleaving writes across a
//! whole multi-transaction run. Serializing whole runs with an OS advisory
//! lock (the same mechanism as `storage::git_notes::lock`) closes that
//! window without needing SQLite itself to change.

use anyhow::Result;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;

const LOCK_FILE_NAME: &str = "index.lock";

/// Held for the lifetime of one `spelunk index` process's DB-writing work.
/// Dropping releases the OS advisory lock (the fd closes), so a killed
/// holder never wedges a future run - there is no stale-lock case to detect
/// or clean up.
pub struct IndexRunLock {
    _file: File,
}

pub enum LockOutcome {
    Acquired(IndexRunLock),
    /// Another process holds the lock. `holder_pid` is best-effort (read
    /// back from the lock file's contents) and purely for the error message
    /// shown to the user - the OS lock itself, not this recorded pid, is
    /// what actually excludes a concurrent writer.
    HeldByOther {
        holder_pid: Option<u32>,
    },
}

/// Try to take the per-project index lock inside `spelunk_dir` (the
/// project's `.spelunk/` directory), non-blocking.
///
/// Non-blocking rather than waited-out (contrast `git_notes::lock`'s bounded
/// poll): an index run's writing window is unbounded - a large repo can
/// embed for minutes - so waiting on it would make a second invocation hang
/// unpredictably instead of failing fast with an actionable message.
pub fn try_acquire(spelunk_dir: &Path) -> Result<LockOutcome> {
    std::fs::create_dir_all(spelunk_dir)?;
    let path = spelunk_dir.join(LOCK_FILE_NAME);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            write!(file, "{}", std::process::id())?;
            file.sync_all().ok();
            Ok(LockOutcome::Acquired(IndexRunLock { _file: file }))
        }
        Err(TryLockError::WouldBlock) => {
            let holder_pid = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse().ok());
            Ok(LockOutcome::HeldByOther { holder_pid })
        }
        Err(TryLockError::Error(e)) => Err(e.into()),
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
        } // guard drops here, releasing the OS lock

        let second = try_acquire(dir.path()).expect("second acquire attempt");
        assert!(
            matches!(second, LockOutcome::Acquired(_)),
            "once the first guard drops, a fresh acquire must succeed"
        );
    }
}
