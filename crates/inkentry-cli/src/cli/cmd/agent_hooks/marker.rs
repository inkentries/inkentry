// Per-session working state for the agent hooks: which repository paths the
// session was already told about, and whether it committed or was already
// prompted to stop. One small line-oriented file per session, appended to, and
// named by the hashed session ref so the raw id never reaches disk. Every write
// is best-effort: a hook must degrade, never fail.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const DIR_NAME: &str = "agent-sessions";
const PATH_LINE: &str = "path ";
const COMMITTED_LINE: &str = "committed";
const STOPPED_LINE: &str = "stopped";

pub(super) fn sessions_dir() -> Option<PathBuf> {
    crate::capability::inkentry_state_dir()
        .ok()
        .map(|state| state.join(DIR_NAME))
}

pub(super) struct SessionMarker {
    file: PathBuf,
    paths: HashSet<String>,
    committed: bool,
    stopped: bool,
}

impl SessionMarker {
    // A missing or unreadable file is a session with nothing recorded yet.
    pub(super) fn open(dir: &Path, session_ref: &str) -> Self {
        let file = dir.join(session_ref);
        let mut marker = Self {
            file,
            paths: HashSet::new(),
            committed: false,
            stopped: false,
        };
        if let Ok(content) = std::fs::read_to_string(&marker.file) {
            marker.apply(&content);
        }
        marker
    }

    fn apply(&mut self, content: &str) {
        for line in content.lines() {
            if let Some(path) = line.strip_prefix(PATH_LINE) {
                self.paths.insert(path.to_string());
            } else if line == COMMITTED_LINE {
                self.committed = true;
            } else if line == STOPPED_LINE {
                self.stopped = true;
            }
        }
    }

    pub(super) fn has_path(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    pub(super) fn edited(&self) -> bool {
        !self.paths.is_empty()
    }

    pub(super) fn committed(&self) -> bool {
        self.committed
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped
    }

    // A path holding a line break cannot be stored on one line: it is never
    // remembered, so it is simply looked up again on its next edit.
    pub(super) fn record_path(&mut self, path: &str) -> bool {
        if path.contains(['\n', '\r']) {
            return false;
        }
        self.paths.insert(path.to_string());
        self.append(&format!("{PATH_LINE}{path}"))
    }

    pub(super) fn record_committed(&mut self) -> bool {
        self.committed = true;
        self.append(COMMITTED_LINE)
    }

    pub(super) fn record_stopped(&mut self) -> bool {
        self.stopped = true;
        self.append(STOPPED_LINE)
    }

    fn append(&self, line: &str) -> bool {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.file)?;
            file.write_all(format!("{line}\n").as_bytes())
        };
        write().is_ok()
    }
}

// Files nobody appended to for `max_age` belong to sessions that are over.
pub(super) fn prune(dir: &Path, max_age: Duration) {
    let Some(cutoff) = SystemTime::now().checked_sub(max_age) else {
        return;
    };
    prune_before(dir, cutoff);
}

fn prune_before(dir: &Path, cutoff: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
            .is_some_and(|modified| modified < cutoff);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_marker_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let marker = SessionMarker::open(dir.path(), "abc");
        assert!(!marker.edited());
        assert!(!marker.committed());
        assert!(!marker.stopped());
        assert!(!marker.has_path("src/lib.rs"));
    }

    #[test]
    fn recorded_state_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut marker = SessionMarker::open(dir.path(), "abc");
        assert!(marker.record_path("src/lib.rs"));
        assert!(marker.record_path("src/main.rs"));
        assert!(marker.record_committed());
        assert!(marker.record_stopped());

        let reopened = SessionMarker::open(dir.path(), "abc");
        assert!(reopened.has_path("src/lib.rs"));
        assert!(reopened.has_path("src/main.rs"));
        assert!(!reopened.has_path("src/other.rs"));
        assert!(reopened.edited());
        assert!(reopened.committed());
        assert!(reopened.stopped());
    }

    #[test]
    fn sessions_do_not_share_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        SessionMarker::open(dir.path(), "one").record_path("a.rs");
        assert!(!SessionMarker::open(dir.path(), "two").has_path("a.rs"));
    }

    #[test]
    fn a_path_with_spaces_round_trips_and_one_with_a_newline_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let mut marker = SessionMarker::open(dir.path(), "abc");
        assert!(marker.record_path("my dir/a b.rs"));
        assert!(!marker.record_path("bad\nname.rs"));
        let reopened = SessionMarker::open(dir.path(), "abc");
        assert!(reopened.has_path("my dir/a b.rs"));
        assert!(!reopened.has_path("bad\nname.rs"));
        assert!(!reopened.has_path("name.rs"));
    }

    #[test]
    fn an_unwritable_marker_reports_failure_but_still_remembers_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let mut marker = SessionMarker::open(&blocker, "abc");
        assert!(!marker.record_path("a.rs"));
        assert!(!marker.record_stopped());
        assert!(marker.has_path("a.rs"));
    }

    #[test]
    fn unrecognised_lines_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("abc"),
            "garbage\npath a.rs\n\u{0}\nstopped\n",
        )
        .unwrap();
        let marker = SessionMarker::open(dir.path(), "abc");
        assert!(marker.has_path("a.rs"));
        assert!(marker.stopped());
        assert!(!marker.committed());
    }

    #[test]
    fn prune_removes_only_files_older_than_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let fresh = dir.path().join("fresh");
        std::fs::write(&old, "stopped\n").unwrap();
        std::fs::write(&fresh, "stopped\n").unwrap();
        let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(ten_days_ago)
            .unwrap();

        prune(dir.path(), Duration::from_secs(7 * 24 * 3600));

        assert!(!old.exists(), "a stale marker is removed");
        assert!(fresh.exists(), "a recent marker is kept");
    }

    #[test]
    fn pruning_a_missing_directory_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        prune(&dir.path().join("absent"), Duration::from_secs(1));
    }
}
