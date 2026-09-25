use super::color::cprintln;
use anyhow::{Context, Result};
use clap::Args;
use inkentry_core::config::DEFAULT_SERVER_PORT;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Also install the post-commit git hook
    #[arg(long)]
    pub hook: bool,

    /// Skip the initial index run
    #[arg(long)]
    pub no_index: bool,

    /// Explicit project slug, written to `.inkentry/config.toml`. Overrides the
    /// git-derived default; use it for projects without a git remote. Ignored
    /// when a `project_id` is already set in config.
    #[arg(long)]
    pub name: Option<String>,
}

use crate::{
    capability,
    config::Config,
    registry::Registry,
    storage::{Database, RewriteRefStatus, ensure_notes_rewrite_ref},
};

use super::memory::reconcile::GitNotesImport;

pub async fn init(args: InitArgs, cfg: Config) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let git_root = find_git_root(&cwd);

    let project_root = match &git_root {
        Some(root) => root.clone(),
        None => {
            eprintln!(
                "Warning: not inside a git repository. Using current directory as project root."
            );
            cwd.clone()
        }
    };

    let inkentry_dir = project_root.join(".inkentry");
    let db_path = inkentry_dir.join("index.db");
    let config_path = inkentry_dir.join("config.toml");

    write_inkentry_gitignore(&inkentry_dir);

    // Never overwrites an existing project_id: no retroactive rename.
    let desired_slug = args
        .name
        .clone()
        .unwrap_or_else(|| inkentry_core::config::derive_project_id(&project_root));
    let (project_slug, wrote_slug) =
        match inkentry_core::config::write_project_slug(&config_path, &desired_slug) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Warning: could not write project slug to config: {e}");
                (desired_slug.clone(), false)
            }
        };

    let already_exists = db_path.exists();
    if already_exists {
        println!(
            "Note: inkentry is already initialised for '{}' (DB exists at {}).",
            project_slug,
            db_path.display()
        );
        println!("Re-running init is safe — it will update the registry and optionally re-index.");
    }

    let root_canonical = inkentry_core::utils::canonicalize(project_root.as_ref());

    if let Ok(reg) = Registry::open() {
        let db_canonical = if db_path.exists() {
            inkentry_core::utils::canonicalize(db_path.as_ref())
        } else {
            db_path.clone()
        };
        if let Err(e) = reg.register(&root_canonical, &db_canonical) {
            eprintln!("Warning: registry update failed: {e}");
        }
    }

    let hook_status = if args.hook {
        match install_hook_for_init() {
            Ok(msg) => msg,
            Err(e) => format!("failed: {e}"),
        }
    } else {
        "not installed  (run `inkentry hooks install` to add)".to_string()
    };

    // Non-interactive (CI / hook) only probes, never auto-spawns.
    //
    // Runs before the index step: the detached embed would otherwise probe a
    // server this command has not started yet and ship a zero-embedding index.
    let server_line: Option<String> = {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            // Keep on one line: `daemon_spawn_call_sites` reads call sites line by line.
            match super::server::ensure_server_running(DEFAULT_SERVER_PORT, &cfg).await {
                Ok((port, true)) => Some(format!(
                    "http://127.0.0.1:{port}  \x1b[32m✓\x1b[0m  (auto-started)"
                )),
                Ok((port, false)) => Some(format!("http://127.0.0.1:{port}  \x1b[32m✓\x1b[0m")),
                Err(e) => {
                    tracing::debug!("server auto-start skipped: {e}");
                    None
                }
            }
        } else {
            let tier = capability::get_tier(&cfg).await;
            match tier {
                capability::Tier::Server { url, .. } => Some(format!("{url}  \x1b[32m✓\x1b[0m")),
                capability::Tier::Offline(_) => {
                    Some("[server not running - semantic search skipped]".to_string())
                }
            }
        }
    };

    let (file_count, chunk_count) = if args.no_index {
        println!("Skipping index (--no-index). Run `inkentry index .` when ready.");
        if db_path.exists() {
            match Database::open(&db_path) {
                Ok(db) => match db.stats() {
                    Ok(stats) => (stats.file_count, stats.chunk_count),
                    Err(_) => (0, 0),
                },
                Err(_) => (0, 0),
            }
        } else {
            (0, 0)
        }
    } else {
        let index_args = super::index::IndexArgs {
            path: project_root.clone(),
            db: None,
            batch_size: 32,
            force: false,
            recount: false,
            no_summaries: false,
            background_phases: false,
            embed_phases: false,
            detach: false,
            // The embed pass is long: hand it to the background worker so init
            // returns after parsing.
            detach_embed: true,
            // `InitArgs` carries no `--config` to forward, so the detached
            // embed child uses the default config.
            config_path: None,
        };
        super::index::index(index_args, cfg.clone()).await?;

        match Database::open(&db_path) {
            Ok(db) => match db.stats() {
                Ok(stats) => (stats.file_count, stats.chunk_count),
                Err(_) => (0, 0),
            },
            Err(_) => (0, 0),
        }
    };

    // Order is load-bearing: on a fresh clone the import must run after the
    // refspec is configured and a fetch has populated the tracking ref, or one
    // `init` imports nothing.
    let notes_lines = if git_root.is_some() {
        configure_notes_refspec(&project_root).await
    } else {
        Vec::new()
    };

    // Non-fatal throughout: a failure here (offline included) must not sink init.
    let memory_line: Option<String> = if let Some(git_root) = git_root.as_ref() {
        let mem_path = inkentry_dir.join("memory.db");
        fetch_notes_best_effort(&project_root).await;
        // Merge the tracking ref first so teammates' entries import too.
        crate::storage::merge_tracking_notes(Some(git_root)).await;
        match super::memory::reconcile::import_git_notes_into_memory(git_root, &mem_path).await {
            Ok(outcome) => git_notes_import_line(&outcome),
            Err(e) => {
                tracing::warn!("git-notes memory import skipped (non-fatal): {e}");
                None
            }
        }
    } else {
        None
    };

    println!();
    println!("inkentry initialised for {}", project_slug);
    println!();
    println!("  Index:   {} files, {} chunks", file_count, chunk_count);
    println!("  DB:      {}", db_path.display());
    if wrote_slug {
        println!(
            "  Project: {}  (written to {})",
            project_slug,
            config_path.display()
        );
    } else {
        println!(
            "  Project: {}  (from {})",
            project_slug,
            config_path.display()
        );
    }
    if wrote_slug {
        println!(
            "           wrote .inkentry/config.toml — commit it so your project slug \
             travels with the repo"
        );
    }
    println!("  Hook:    {}", hook_status);
    if let Some(line) = memory_line {
        println!("  Memory:  {line}");
    }
    if let Some(line) = server_line {
        cprintln!("  Server:  {line}");
    }
    for line in &notes_lines {
        println!("  {line}");
    }
    println!();
    println!("Next steps:");
    println!("  inkentry search \"your query\"");
    println!("  inkentry context");

    Ok(())
}

// Skipped edges are always reported: a graph thinner than the one on the ref
// would otherwise be found only by missing a link.
fn git_notes_import_line(outcome: &GitNotesImport) -> Option<String> {
    let skipped = |u: usize| {
        let noun = if u == 1 { "edge" } else { "edges" };
        format!(
            "{u} {noun} skipped (the entries they point at are not here yet; \
             a later import resolves them)"
        )
    };
    // Users cannot tell a skipped supersede edge from any other skipped link.
    let unresolved = outcome.edges_unresolved + outcome.supersede_edges_unresolved;
    match (outcome.imported, unresolved) {
        (0, 0) => None,
        (0, u) => Some(skipped(u)),
        (n, 0) => Some(format!(
            "imported {n} entries from git notes\n{SUMMARY_CONTINUATION}{MINTED_HERE}"
        )),
        (n, u) => Some(format!(
            "imported {n} entries from git notes, {}\n{SUMMARY_CONTINUATION}{MINTED_HERE}",
            skipped(u)
        )),
    }
}

const MINTED_HERE: &str = "the ids these entries show were minted on this machine; \
     quote the entity id from `inkentry memory show` to name an entry anywhere else";

const SUMMARY_CONTINUATION: &str = "           ";

// Written only when absent, so re-init never clobbers user edits.
fn write_inkentry_gitignore(inkentry_dir: &std::path::Path) {
    let gitignore_path = inkentry_dir.join(".gitignore");
    if gitignore_path.exists() {
        return;
    }
    // config.toml is committed, so it must stay out of this list.
    const GITIGNORE: &str = "# Machine-specific SQLite, regenerated by `inkentry index`.\n\
                             index.db*\n\
                             memory.db*\n\
                             # Per-run index lock + its pid sidecar (holds a local process id).\n\
                             index.lock*\n\
                             # Diagnostics from detached background runs.\n\
                             *.log\n";
    if let Err(e) = std::fs::create_dir_all(inkentry_dir) {
        eprintln!("Warning: could not create {}: {e}", inkentry_dir.display());
        return;
    }
    if let Err(e) = std::fs::write(&gitignore_path, GITIGNORE) {
        eprintln!("Warning: could not write {}: {e}", gitignore_path.display());
    }
}

async fn fetch_notes_best_effort(project_root: &std::path::Path) {
    use std::process::Stdio;
    const NOTES_FETCH_REFSPEC: &str = "+refs/notes/inkentry*:refs/notes/origin/inkentry*";
    const FETCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

    let has_origin = tokio::process::Command::new("git")
        .current_dir(project_root)
        .args(["remote", "get-url", "origin"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !has_origin {
        return;
    }

    let mut child = match tokio::process::Command::new("git")
        .current_dir(project_root)
        .args(["fetch", "origin", NOTES_FETCH_REFSPEC])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("init notes fetch skipped (spawn failed): {e}");
            return;
        }
    };
    // On timeout the future is dropped and `kill_on_drop` reaps the child, so a
    // hung fetch cannot leave `init` waiting or orphan a git process.
    if tokio::time::timeout(FETCH_BUDGET, child.wait())
        .await
        .is_err()
    {
        tracing::debug!("init notes fetch timed out; continuing (read paths still import later)");
    }
}

// The destination is a tracking ref, never the working ref: fetching straight
// onto `refs/notes/inkentry` force-updates it and replaces a local unpushed
// note. The glob form keeps plain `git fetch` from exiting 128 while the remote
// ref does not exist.
//
// No push refspec: any `remote.origin.push` overrides git's default branch
// push. Publishing rides the opt-in pre-push hook, announced below so it is
// discoverable.
//
// `notes.rewriteRef` is set even without an `origin`, since rewrites are local.
async fn configure_notes_refspec(project_root: &std::path::Path) -> Vec<String> {
    const FETCH_REFSPEC: &str = "+refs/notes/inkentry*:refs/notes/origin/inkentry*";

    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(project_root)
            .args(args)
            .output()
    };

    let mut lines = {
        let has_origin = git(&["remote", "get-url", "origin"])
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_origin {
            vec![
                "Memory:  no 'origin' remote, so the notes refspec is not configured".to_string(),
                format!(
                    "         run later: git config --add remote.origin.fetch '{FETCH_REFSPEC}'"
                ),
            ]
        } else {
            let already = git(&["config", "--get-all", "remote.origin.fetch"])
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .any(|l| l.trim() == FETCH_REFSPEC)
                })
                .unwrap_or(false);
            if already {
                vec!["Memory:  notes fetch refspec already configured on 'origin'".to_string()]
            } else {
                match git(&["config", "--add", "remote.origin.fetch", FETCH_REFSPEC]) {
                    Ok(o) if o.status.success() => vec![
                        "Memory:  configured notes fetch refspec on 'origin' (teammates' memory arrives on fetch)"
                            .to_string(),
                    ],
                    Ok(o) => vec![format!(
                        "Memory:  could not configure notes refspec: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    )],
                    Err(e) => vec![format!("Memory:  could not configure notes refspec: {e}")],
                }
            }
        }
    };

    // Publishing is opt-in, so say so unprompted.
    if super::hooks::pre_push_installed(project_root) {
        lines.push(
            "         pre-push hook installed: your memory publishes on `git push`".to_string(),
        );
    } else {
        lines.push(format!(
            "         your memory stays local until you install the pre-push hook: {}",
            super::hooks::PRE_PUSH_INSTALL_CMD
        ));
    }

    match ensure_notes_rewrite_ref(Some(project_root)).await {
        RewriteRefStatus::Configured => lines.push(
            "         configured notes.rewriteRef (memory survives `git commit --amend` and `git rebase`)"
                .to_string(),
        ),
        RewriteRefStatus::AlreadyCovered => {}
        RewriteRefStatus::Failed => {
            lines.push(
                "         could not set notes.rewriteRef; memory will not survive `git commit --amend` or `git rebase`"
                    .to_string(),
            );
            lines.push(
                "         run later: git config --add notes.rewriteRef refs/notes/inkentry"
                    .to_string(),
            );
        }
    }
    lines
}

fn find_git_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// Reuses `hooks` path resolution; a hardcoded `$GIT_DIR/hooks` would ignore `core.hooksPath`.
fn install_hook_for_init() -> Result<String> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    match super::hooks::install_post_commit_hook(&cwd)? {
        super::hooks::Installed::Wrote(p) => Ok(format!("installed at {}", p.display())),
        super::hooks::Installed::Updated(p) => Ok(format!("updated at {}", p.display())),
        super::hooks::Installed::AlreadyPresent(p) => {
            Ok(format!("already installed at {}", p.display()))
        }
    }
}

#[cfg(test)]
mod git_notes_import_line_tests {
    use super::*;

    #[test]
    fn a_skipped_edge_is_reported_whether_or_not_entries_arrived() {
        assert_eq!(git_notes_import_line(&GitNotesImport::default()), None);

        let entries_only = GitNotesImport {
            imported: 3,
            edges_applied: 2,
            edges_unresolved: 0,
            ..Default::default()
        };
        let line = git_notes_import_line(&entries_only).expect("entries must be reported");
        // Spelled out in one piece: an expected value built with a line
        // continuation would reproduce the stray-spaces bug instead of catching it.
        assert_eq!(
            line,
            "imported 3 entries from git notes\n           the ids these entries show were minted on this machine; quote the entity id from `inkentry memory show` to name an entry anywhere else"
        );

        let edges_only = GitNotesImport {
            imported: 0,
            edges_applied: 0,
            edges_unresolved: 1,
            ..Default::default()
        };
        let line = git_notes_import_line(&edges_only).expect("a skipped edge must be reported");
        assert!(line.starts_with("1 edge skipped"), "{line}");

        let both = GitNotesImport {
            imported: 2,
            edges_applied: 1,
            edges_unresolved: 2,
            ..Default::default()
        };
        let line = git_notes_import_line(&both).expect("both halves must be reported");
        assert!(line.contains("imported 2 entries"), "{line}");
        assert!(line.contains("2 edges skipped"), "{line}");
        assert!(line.contains("minted on this machine"), "{line}");
        // Only the alignment indent may hold a run of spaces.
        for rendered in line.lines().map(|l| l.trim_start()) {
            assert!(!rendered.contains("  "), "doubled space in {rendered:?}");
        }
    }

    #[test]
    fn a_skipped_supersede_edge_is_counted_with_the_rest() {
        let supersede_only = GitNotesImport {
            supersede_edges_unresolved: 1,
            ..Default::default()
        };
        let line = git_notes_import_line(&supersede_only)
            .expect("a skipped supersede edge must be reported");
        assert!(line.starts_with("1 edge skipped"), "{line}");

        let mixed = GitNotesImport {
            edges_unresolved: 1,
            supersede_edges_unresolved: 1,
            ..Default::default()
        };
        let line = git_notes_import_line(&mixed).expect("mixed skips reported");
        assert!(line.starts_with("2 edges skipped"), "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_ignores_dbs_but_not_committed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let inkentry_dir = tmp.path().join(".inkentry");

        write_inkentry_gitignore(&inkentry_dir);

        let body = std::fs::read_to_string(inkentry_dir.join(".gitignore")).unwrap();
        assert!(body.contains("index.db*"), "must ignore index.db*: {body}");
        assert!(
            body.contains("memory.db*"),
            "must ignore memory.db*: {body}"
        );
        assert!(
            !body.contains("config.toml"),
            "config.toml is committed, must not be ignored: {body}"
        );
    }

    #[test]
    fn gitignore_ignores_index_run_lock_and_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let inkentry_dir = tmp.path().join(".inkentry");

        write_inkentry_gitignore(&inkentry_dir);

        let body = std::fs::read_to_string(inkentry_dir.join(".gitignore")).unwrap();
        // `index.lock.pid` holds a machine-local pid; committing it churns across machines.
        assert!(
            body.contains("index.lock*"),
            "must ignore the index run-lock + pid sidecar: {body}"
        );
    }

    #[test]
    fn generated_gitignore_makes_git_ignore_lock_and_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Drops global/system git config so a developer's core.excludesfile
        // can neither mask nor manufacture the ignore.
        crate::cli::cmd::test_support::isolate_git_config();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(repo)
                .args(args)
                .output()
                .expect("spawn git")
        };
        assert!(
            git(&["init", "-q", "-b", "main"]).status.success(),
            "git init failed"
        );

        let inkentry_dir = repo.join(".inkentry");
        write_inkentry_gitignore(&inkentry_dir);

        for f in [
            "index.lock",
            "index.lock.pid",
            "index.db",
            "memory.db",
            "index.log",
        ] {
            std::fs::write(inkentry_dir.join(f), b"x").unwrap();
        }

        for rel in [".inkentry/index.lock", ".inkentry/index.lock.pid"] {
            assert!(
                git(&["check-ignore", "-q", rel]).status.success(),
                "{rel} must be git-ignored by the generated .gitignore"
            );
        }

        // `-uall` lists untracked files individually instead of collapsing `.inkentry/`.
        let out = git(&["status", "--porcelain", "-uall"]).stdout;
        let porcelain = String::from_utf8_lossy(&out);
        for f in [
            "index.lock",
            "index.lock.pid",
            "index.db",
            "memory.db",
            "index.log",
        ] {
            assert!(
                !porcelain.contains(f),
                "{f} must not appear in `git status --porcelain`, got:\n{porcelain}"
            );
        }
        assert!(
            porcelain.contains(".gitignore"),
            "the generated .gitignore should stay untracked+committable, got:\n{porcelain}"
        );
    }

    #[test]
    fn gitignore_is_idempotent_and_preserves_user_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let inkentry_dir = tmp.path().join(".inkentry");
        std::fs::create_dir_all(&inkentry_dir).unwrap();
        let gitignore_path = inkentry_dir.join(".gitignore");
        std::fs::write(&gitignore_path, "custom-user-line\n").unwrap();

        write_inkentry_gitignore(&inkentry_dir);

        let body = std::fs::read_to_string(&gitignore_path).unwrap();
        assert_eq!(body, "custom-user-line\n");
    }
}
