/// If `root` is a git worktree (`.git` is a file, not a directory), ensure that
/// `.spelunk` is a symlink pointing at the main worktree's `.spelunk` folder so
/// all worktrees share one index.  SQLite WAL mode handles concurrent access from
/// multiple processes safely.
///
/// No-ops if the symlink (or a real `.spelunk` dir) already exists, or if the main
/// worktree hasn't been indexed yet (its `.spelunk` folder doesn't exist).
pub(super) fn ensure_spelunk_symlink(root: &std::path::Path) {
    let spelunk = root.join(".spelunk");
    // Already present (real dir or existing symlink) — nothing to do.
    if spelunk.exists() || spelunk.is_symlink() {
        return;
    }

    // A git worktree has a `.git` *file* (not a directory) containing a pointer
    // to the worktrees entry inside the main repo's .git directory.
    let git = root.join(".git");
    if !git.is_file() {
        return;
    }

    let Ok(content) = std::fs::read_to_string(&git) else {
        return;
    };
    // Content is: "gitdir: /abs/path/to/main/.git/worktrees/<name>\n"
    let Some(gitdir_str) = content.strip_prefix("gitdir:") else {
        return;
    };
    let gitdir = std::path::PathBuf::from(gitdir_str.trim());

    // Validate expected structure: <main>/.git/worktrees/<name>
    // parent()  → worktrees/
    // parent()  → .git/
    // parent()  → main worktree root
    let Some(worktrees_dir) = gitdir.parent() else {
        return;
    };
    if worktrees_dir.file_name() != Some(std::ffi::OsStr::new("worktrees")) {
        return; // unexpected gitdir layout — don't guess
    }
    let Some(main_root) = worktrees_dir.parent().and_then(|p| p.parent()) else {
        return;
    };

    // Guard: never create a symlink that points to itself.
    if main_root == root {
        return;
    }

    let main_spelunk = main_root.join(".spelunk");
    if !main_spelunk.exists() {
        return; // main worktree not yet indexed — skip
    }

    #[cfg(unix)]
    {
        match std::os::unix::fs::symlink(&main_spelunk, &spelunk) {
            Ok(()) => eprintln!(
                "Created .spelunk symlink → {} (shared worktree index)",
                main_spelunk.display()
            ),
            Err(e) => tracing::warn!("could not create .spelunk symlink in worktree: {e}"),
        }
    }
}
