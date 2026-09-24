//! Linked-file path normalisation and state resolution.
//!
//! A raw path is made repository-relative and forward-slashed; one that escapes
//! the project root is refused. Its [`FileState`] comes from git, falling back
//! to a disk-only check when git is unavailable. A missing file is reported,
//! not refused.

use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

use crate::storage::note_record::now_secs;

/// Where a linked file stands relative to the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// Known to git at `HEAD`.
    Tracked,
    /// Exists on disk but not (yet) tracked.
    Untracked,
    /// Neither tracked nor present on disk.
    Missing,
}

impl FileState {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileState::Tracked => "tracked",
            FileState::Untracked => "untracked",
            FileState::Missing => "missing",
        }
    }
}

/// A path resolved against a project root, with the state it was in at
/// `checked_at` (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFileLink {
    /// Repository-relative, forward-slashed, no leading `./`.
    pub path: String,
    pub state: FileState,
    pub checked_at: i64,
}

/// Normalises `raw` against `root` and resolves its state.
///
/// `root` need not exist on disk; the state then degrades to
/// [`FileState::Missing`]. Errors when `raw` resolves outside `root`.
pub fn resolve_file_link(root: &Path, raw: &str) -> Result<ResolvedFileLink> {
    let path = normalize_relative_path(root, raw)?;
    let state = determine_state(root, &path);
    Ok(ResolvedFileLink {
        path,
        state,
        checked_at: now_secs(),
    })
}

/// The repository-relative, forward-slashed, `./`-stripped form of `raw`.
///
/// Purely lexical, so a path that does not exist yet still resolves. Errors when
/// the result would lie outside `root`.
pub fn normalize_relative_path(root: &Path, raw: &str) -> Result<String> {
    let slashed = raw.trim().replace('\\', "/");
    anyhow::ensure!(!slashed.is_empty(), "linked file path is empty");

    let candidate = if Path::new(&slashed).is_absolute() {
        PathBuf::from(&slashed)
    } else {
        root.join(&slashed)
    };

    let root_clean = lexically_normalize(root);
    let candidate_clean = lexically_normalize(&candidate);

    let rel = candidate_clean.strip_prefix(&root_clean).with_context(|| {
        format!(
            "linked file path '{raw}' escapes the project root {}",
            root.display()
        )
    })?;

    let joined: Vec<&str> = rel
        .components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("linked file path '{raw}' is not valid UTF-8"))
        })
        .collect::<Result<_>>()?;
    anyhow::ensure!(
        !joined.is_empty(),
        "linked file path '{raw}' resolves to the project root itself"
    );
    Ok(joined.join("/"))
}

// Not `std::fs::canonicalize`: a linked file may legitimately not exist yet.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn determine_state(root: &Path, rel_path: &str) -> FileState {
    if is_tracked_at_head(root, rel_path) {
        return FileState::Tracked;
    }
    if root.join(rel_path).exists() {
        FileState::Untracked
    } else {
        FileState::Missing
    }
}

// `rel_path` follows `--` and bypasses the shell, so it is never read as a flag.
fn is_tracked_at_head(root: &Path, rel_path: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(rel_path)
        .output()
        .is_ok_and(|out| out.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_relative_path_passes_through() {
        let root = Path::new("/repo");
        assert_eq!(
            normalize_relative_path(root, "src/lib.rs").unwrap(),
            "src/lib.rs"
        );
    }

    #[test]
    fn a_leading_dot_slash_is_stripped() {
        let root = Path::new("/repo");
        assert_eq!(
            normalize_relative_path(root, "./src/lib.rs").unwrap(),
            "src/lib.rs"
        );
    }

    #[test]
    fn backslashes_become_forward_slashes() {
        let root = Path::new("/repo");
        assert_eq!(
            normalize_relative_path(root, "src\\lib.rs").unwrap(),
            "src/lib.rs"
        );
    }

    #[test]
    fn an_absolute_path_inside_the_root_becomes_relative() {
        let root = Path::new("/repo");
        assert_eq!(
            normalize_relative_path(root, "/repo/src/lib.rs").unwrap(),
            "src/lib.rs"
        );
    }

    #[test]
    fn a_path_escaping_the_root_via_dotdot_is_an_error() {
        let root = Path::new("/repo/project");
        let err = normalize_relative_path(root, "../../etc/passwd").unwrap_err();
        assert!(
            err.to_string().contains("escapes the project root"),
            "{err}"
        );
    }

    #[test]
    fn an_absolute_path_outside_the_root_is_an_error() {
        let root = Path::new("/repo/project");
        let err = normalize_relative_path(root, "/etc/passwd").unwrap_err();
        assert!(
            err.to_string().contains("escapes the project root"),
            "{err}"
        );
    }

    #[test]
    fn a_path_that_resolves_to_the_root_itself_is_an_error() {
        let root = Path::new("/repo/project");
        assert!(normalize_relative_path(root, ".").is_err());
        assert!(normalize_relative_path(root, "sub/..").is_err());
    }

    #[test]
    fn internal_dotdot_that_stays_inside_the_root_is_resolved() {
        let root = Path::new("/repo/project");
        assert_eq!(
            normalize_relative_path(root, "src/../lib/mod.rs").unwrap(),
            "lib/mod.rs"
        );
    }

    #[test]
    fn a_file_that_exists_on_disk_but_is_not_git_tracked_is_untracked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("scratch.txt"), b"x").unwrap();
        let link = resolve_file_link(dir.path(), "scratch.txt").unwrap();
        assert_eq!(link.path, "scratch.txt");
        assert_eq!(link.state, FileState::Untracked);
    }

    #[test]
    fn a_file_absent_from_disk_and_git_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let link = resolve_file_link(dir.path(), "does/not/exist.rs").unwrap();
        assert_eq!(link.state, FileState::Missing);
    }

    #[test]
    fn a_non_git_project_derives_state_from_disk_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("present.txt"), b"x").unwrap();
        assert_eq!(
            resolve_file_link(dir.path(), "present.txt").unwrap().state,
            FileState::Untracked
        );
        assert_eq!(
            resolve_file_link(dir.path(), "absent.txt").unwrap().state,
            FileState::Missing
        );
    }

    #[test]
    fn a_file_committed_to_git_is_tracked() {
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::isolate_git_config();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git command")
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join("tracked.rs"), b"fn main() {}").unwrap();
        run(&["add", "tracked.rs"]);
        run(&[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ]);

        let link = resolve_file_link(dir.path(), "tracked.rs").unwrap();
        assert_eq!(link.state, FileState::Tracked);
    }

    #[test]
    fn checked_at_is_populated() {
        let dir = tempfile::tempdir().unwrap();
        let link = resolve_file_link(dir.path(), "x.rs").unwrap();
        assert!(link.checked_at > 0);
    }
}
