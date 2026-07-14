//! Cross-process lock serializing the `refs/notes/spelunk` read-modify-write.
//!
//! Git's own ref locking cannot help here: the loss happens at the content
//! layer, not the ref layer, and racing writers each hold the ref lock
//! legitimately in turn. See issue #185 and ADR-069 (D6).

use anyhow::Result;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Lock file name, created inside the git **common** dir.
const LOCK_FILE_NAME: &str = "spelunk-notes.lock";

/// Bounded wait before giving up on a contended lock. Each holder keeps the
/// lock for one read-modify-write (a few git subprocesses, ~30ms), so this is
/// orders of magnitude above realistic contention; reaching it means something
/// pathological, not a busy repo.
const LOCK_WAIT_BUDGET: Duration = Duration::from_secs(5);

/// Poll interval while the lock is held by another process.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Holds the notes lock for its lifetime; the lock is released on drop.
#[derive(Debug)]
pub struct NotesLock {
    // Dropping the File closes the fd, which releases the OS lock.
    _file: File,
}

/// Resolve the lock path: `<git-common-dir>/spelunk-notes.lock`.
///
/// The **common** dir, not the per-worktree git dir: worktrees share one
/// `refs/notes/spelunk`, so a per-worktree lock would fail to serialize the
/// actual contenders.
async fn notes_lock_path(git_root: Option<&Path>) -> Result<PathBuf> {
    let raw = super::run_git(git_root, &["rev-parse", "--git-common-dir"]).await?;
    let raw = Path::new(raw.trim());

    // git may answer with a path relative to the dir it ran in.
    let common_dir = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        match git_root {
            Some(root) => root.join(raw),
            None => std::env::current_dir()?.join(raw),
        }
    };

    Ok(common_dir.join(LOCK_FILE_NAME))
}

/// Acquire the notes lock, waiting up to [`LOCK_WAIT_BUDGET`].
///
/// Returns `None` if the lock could not be taken (contended past the budget,
/// or unusable — e.g. a read-only or lock-hostile filesystem). Callers must
/// treat `None` as "proceed without serialization" or "skip the optional
/// work", never as a hard error: a caller's command must never fail because of
/// lock contention.
///
/// Held across the whole read-modify-write, not just the write: the race is
/// the gap between reading the note body and writing it back.
pub async fn lock_notes(git_root: Option<&Path>) -> Option<NotesLock> {
    let path = match notes_lock_path(git_root).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("could not resolve git notes lock path ({e}); proceeding unlocked");
            return None;
        }
    };

    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "could not open git notes lock {}: {e}; proceeding unlocked",
                path.display()
            );
            return None;
        }
    };

    let deadline = Instant::now() + LOCK_WAIT_BUDGET;
    loop {
        match file.try_lock() {
            Ok(()) => return Some(NotesLock { _file: file }),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    tracing::warn!(
                        "git notes lock still held after {:?}; proceeding unlocked",
                        LOCK_WAIT_BUDGET
                    );
                    return None;
                }
                tokio::time::sleep(LOCK_POLL_INTERVAL).await;
            }
            Err(TryLockError::Error(e)) => {
                tracing::warn!("git notes lock unusable ({e}); proceeding unlocked");
                return None;
            }
        }
    }
}
