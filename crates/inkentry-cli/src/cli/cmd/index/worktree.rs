/// If `root` is a git linked worktree, returns the main worktree root.
/// Otherwise returns `root` itself.
///
/// No file is created or modified. Callers should use the returned path when
/// constructing the default `.inkentry/index.db` path so that all worktrees
/// share one index without needing a symlink.
pub(super) fn resolve_main_worktree_root(root: &std::path::Path) -> std::path::PathBuf {
    inkentry_core::utils::resolve_main_worktree_root(root)
}
