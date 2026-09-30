// Maps the path an agent is about to edit onto the repository-relative form
// `memory add --files` stored, so the two match exactly.

use std::path::{Path, PathBuf};

use crate::storage::normalize_relative_path;

// `roots` are tried in order: the memory store's project root, then the
// current linked worktree's own root, where an edit in a linked worktree lands
// outside the main checkout the store lives in. `None` for a path outside
// every root.
pub(super) fn repo_relative(roots: &[&Path], cwd: &Path, raw: &str) -> Option<String> {
    let slashed = raw.trim().replace('\\', "/");
    if slashed.is_empty() {
        return None;
    }
    let absolute = if Path::new(&slashed).is_absolute() {
        PathBuf::from(&slashed)
    } else {
        cwd.join(&slashed)
    };
    if let Some(found) = first_match(roots, &absolute) {
        return Some(found);
    }
    // The agent and the store may spell one location differently (a symlinked
    // temp or home directory). The file itself may not exist yet, so only its
    // directory is resolved.
    let canonical_roots: Vec<PathBuf> = roots
        .iter()
        .map(|root| inkentry_core::utils::canonicalize(root))
        .collect();
    let canonical_root_refs: Vec<&Path> = canonical_roots.iter().map(PathBuf::as_path).collect();
    let canonical_file = match (absolute.parent(), absolute.file_name()) {
        (Some(dir), Some(name)) => inkentry_core::utils::canonicalize(dir).join(name),
        _ => return None,
    };
    first_match(&canonical_root_refs, &canonical_file)
}

fn first_match(roots: &[&Path], absolute: &Path) -> Option<String> {
    let absolute = absolute.to_str()?;
    roots
        .iter()
        .find_map(|root| normalize_relative_path(root, absolute).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_path_under_the_root_becomes_repo_relative() {
        let root = Path::new("/repo");
        assert_eq!(
            repo_relative(&[root], Path::new("/repo"), "/repo/src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn a_relative_path_is_taken_from_the_working_directory() {
        let root = Path::new("/repo");
        assert_eq!(
            repo_relative(&[root], Path::new("/repo/src"), "lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            repo_relative(&[root], Path::new("/repo/src"), "../Cargo.toml").as_deref(),
            Some("Cargo.toml")
        );
    }

    #[test]
    fn a_path_outside_every_root_is_none() {
        let root = Path::new("/repo");
        assert_eq!(
            repo_relative(&[root], Path::new("/repo"), "/etc/hosts"),
            None
        );
        assert_eq!(
            repo_relative(&[root], Path::new("/repo"), "../elsewhere/a.rs"),
            None
        );
        assert_eq!(repo_relative(&[root], Path::new("/repo"), "  "), None);
        assert_eq!(repo_relative(&[root], Path::new("/repo"), "/repo"), None);
    }

    #[test]
    fn a_later_root_matches_when_an_earlier_one_does_not() {
        let main = Path::new("/main");
        let linked = Path::new("/linked");
        assert_eq!(
            repo_relative(&[main, linked], Path::new("/linked"), "/linked/src/a.rs").as_deref(),
            Some("src/a.rs")
        );
    }

    #[test]
    fn dot_segments_and_backslashes_are_normalised() {
        let root = Path::new("/repo");
        assert_eq!(
            repo_relative(&[root], Path::new("/repo"), "/repo/./src/../src\\lib.rs").as_deref(),
            Some("src/lib.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_spelling_of_the_root_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            repo_relative(&[&real], &link, "src/new.rs").as_deref(),
            Some("src/new.rs")
        );
    }
}
