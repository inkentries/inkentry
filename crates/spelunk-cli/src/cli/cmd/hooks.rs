use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::path::Path;

#[derive(Args, Debug)]
pub struct HooksArgs {
    #[command(subcommand)]
    pub command: HooksCommand,
}

#[derive(Subcommand, Debug)]
pub enum HooksCommand {
    /// Install a post-commit hook that auto-indexes and harvests memory, or a
    /// pre-push hook that publishes memory to the remote (`--pre-push`)
    Install(HooksInstallArgs),
    /// Remove every git hook spelunk installed
    Uninstall,
}

#[derive(Args, Debug)]
pub struct HooksInstallArgs {
    /// Install the pre-push hook that publishes memory notes on `git push`
    #[arg(long, conflicts_with = "ci")]
    pub pre_push: bool,

    /// Print a GitHub Actions workflow step instead of writing a git hook
    #[arg(long)]
    pub ci: bool,
}

pub fn hooks(args: HooksArgs) -> Result<()> {
    match args.command {
        HooksCommand::Install(a) => hooks_install(a),
        HooksCommand::Uninstall => hooks_uninstall(),
    }
}

const POST_COMMIT_HOOK: &str = r#"#!/bin/sh
# spelunk post-commit hook — installed by `spelunk hooks install`
# Keeps the spelunk index in sync and harvests memory from new commits.
# Silently skips if `spelunk` is not in PATH, so teammates without spelunk are unaffected.

if ! command -v spelunk >/dev/null 2>&1; then
  exit 0
fi

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0

spelunk index "$PROJECT_ROOT" --detach
spelunk memory harvest --git-range HEAD~1..HEAD --detach
"#;

/// The pre-push shim. `{spelunk}` is substituted with the shell-quoted absolute
/// path of this binary by [`pre_push_hook_body`].
///
/// Every decision lives in the command, not here: a hook body is a string a user
/// already has on disk, so anything encoded in it cannot be changed by a release.
const PRE_PUSH_HOOK_TEMPLATE: &str = r#"#!/bin/sh
# spelunk pre-push hook (installed by `spelunk hooks install --pre-push`)
# Publishes spelunk memory (refs/notes/spelunk) to the remote you are pushing to,
# so decisions travel with the code they describe.
#
# The path below is absolute rather than a PATH lookup: GUI git clients on macOS
# inherit their environment from launchd, not from your shell profile. If spelunk
# is no longer there this exits 127 and stops the push, which is the intended
# loud failure; re-run `spelunk hooks install --pre-push` to re-resolve it.
#
# `exec` makes this hook's status the command's, and --best-effort makes a failed
# publish exit 0, so publishing can never cost you your push.
# stdout is dropped: the command emits JSONL, which a `git push` should not print.

exec {spelunk} plumbing publish-notes --best-effort "$@" >/dev/null
"#;

const CI_STEP: &str = r#"# Add to your .github/workflows/ file:
- name: Update spelunk index
  run: |
    if command -v spelunk >/dev/null 2>&1; then
      spelunk index . --detach
      spelunk memory harvest --git-range HEAD~1..HEAD --detach
    fi
"#;

/// An installable hook: git's filename for it, and the marker line identifying a
/// spelunk-written copy.
struct HookSpec {
    name: &'static str,
    marker: &'static str,
}

const POST_COMMIT: HookSpec = HookSpec {
    name: "post-commit",
    marker: "spelunk post-commit hook",
};

const PRE_PUSH: HookSpec = HookSpec {
    name: "pre-push",
    marker: "spelunk pre-push hook",
};

/// Every hook `uninstall` considers.
const ALL_HOOKS: [&HookSpec; 2] = [&POST_COMMIT, &PRE_PUSH];

/// The command that installs the pre-push hook. `init` names it when it tells
/// the user their memory stays local until they opt in (ADR-069 D3).
pub const PRE_PUSH_INSTALL_CMD: &str = "spelunk hooks install --pre-push";

/// Quote `path` for a POSIX shell. The shim runs under Git for Windows' `sh`,
/// where single quotes keep backslashes intact, so a Windows path has to arrive
/// forward-slashed.
fn sh_quoted(path: &Path) -> String {
    let forward = path.display().to_string().replace('\\', "/");
    format!("'{}'", forward.replace('\'', r"'\''"))
}

/// The pre-push shim, with this binary's resolved absolute path embedded.
fn pre_push_hook_body() -> Result<String> {
    let exe = std::env::current_exe().context("resolving the path of the spelunk binary")?;
    Ok(PRE_PUSH_HOOK_TEMPLATE.replace("{spelunk}", &sh_quoted(&exe)))
}

/// Whether spelunk's own pre-push hook is installed in the repo holding `dir`.
/// False for a foreign pre-push hook: that one publishes nothing.
pub fn pre_push_installed(dir: &Path) -> bool {
    let Ok(git_dir) = find_git_dir_in(dir) else {
        return false;
    };
    std::fs::read_to_string(git_dir.join("hooks").join(PRE_PUSH.name))
        .is_ok_and(|body| body.contains(PRE_PUSH.marker))
}

fn find_git_dir() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    find_git_dir_in(&cwd)
}

fn find_git_dir_in(dir: &Path) -> Result<std::path::PathBuf> {
    let repo = gix::discover(dir).context("Not inside a git repository.")?;
    Ok(repo.git_dir().to_path_buf())
}

/// What [`write_hook`] did.
enum Installed {
    Wrote(std::path::PathBuf),
    /// Ours, but the body changed: a moved binary re-resolves through here.
    Updated(std::path::PathBuf),
    AlreadyPresent(std::path::PathBuf),
}

/// Write `body` to `<git-dir>/hooks/<spec.name>`, refusing to clobber a hook
/// spelunk did not write.
fn write_hook(spec: &HookSpec, body: &str) -> Result<Installed> {
    let hooks_dir = find_git_dir()?.join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join(spec.name);

    let mut replacing = false;
    if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path)?;
        if !existing.contains(spec.marker) {
            anyhow::bail!(
                "A {} hook already exists at {}.\n\
                 Inspect it and merge manually, or remove it first.",
                spec.name,
                hook_path.display()
            );
        }
        if existing == body {
            return Ok(Installed::AlreadyPresent(hook_path));
        }
        replacing = true;
    }

    std::fs::write(&hook_path, body)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }

    Ok(if replacing {
        Installed::Updated(hook_path)
    } else {
        Installed::Wrote(hook_path)
    })
}

fn hooks_install(args: HooksInstallArgs) -> Result<()> {
    if args.ci {
        print!("{CI_STEP}");
        return Ok(());
    }

    if args.pre_push {
        return install_pre_push();
    }
    install_post_commit()
}

fn install_post_commit() -> Result<()> {
    match write_hook(&POST_COMMIT, POST_COMMIT_HOOK)? {
        Installed::AlreadyPresent(p) => {
            println!("Hook already installed at {}", p.display());
            return Ok(());
        }
        Installed::Updated(p) => println!("Updated post-commit hook at {}", p.display()),
        Installed::Wrote(p) => println!("Installed post-commit hook at {}", p.display()),
    }
    println!("After each commit, spelunk will:");
    println!("  - Re-index the project");
    println!("  - Harvest memory from the new commit");
    println!("Teammates without spelunk installed are unaffected.");
    Ok(())
}

fn install_pre_push() -> Result<()> {
    match write_hook(&PRE_PUSH, &pre_push_hook_body()?)? {
        Installed::AlreadyPresent(p) => {
            println!("Hook already installed at {}", p.display());
            return Ok(());
        }
        Installed::Updated(p) => println!("Updated pre-push hook at {}", p.display()),
        Installed::Wrote(p) => println!("Installed pre-push hook at {}", p.display()),
    }
    println!("On each `git push`, spelunk will publish your memory to that remote:");
    println!("  - Fetch and merge teammates' memory notes (union, nothing dropped)");
    println!("  - Push refs/notes/spelunk alongside the code you are pushing");
    println!("Your push is never blocked: on failure the hook warns and exits 0.");
    println!("Teammates never receive this hook: git does not clone .git/hooks.");
    Ok(())
}

fn hooks_uninstall() -> Result<()> {
    let hooks_dir = find_git_dir()?.join("hooks");
    let mut removed = 0usize;
    let mut foreign: Vec<std::path::PathBuf> = Vec::new();

    for spec in ALL_HOOKS {
        let hook_path = hooks_dir.join(spec.name);
        if !hook_path.exists() {
            continue;
        }
        if !std::fs::read_to_string(&hook_path)?.contains(spec.marker) {
            foreign.push(hook_path);
            continue;
        }
        std::fs::remove_file(&hook_path)?;
        println!("Removed {} hook.", spec.name);
        removed += 1;
    }

    // Only a wholly ineffective uninstall is an error: with a spelunk hook
    // removed, leaving someone else's hook alone is the correct outcome.
    if removed == 0 {
        if let Some(p) = foreign.first() {
            anyhow::bail!(
                "The hook at {} was not installed by spelunk. Remove it manually.",
                p.display()
            );
        }
        println!("No spelunk hooks found.");
        return Ok(());
    }

    for p in &foreign {
        println!(
            "Left {} alone: it was not installed by spelunk.",
            p.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backslash reaches Git Bash intact through single quotes, so a Windows
    /// path embedded raw would resolve to nothing and every push would fail.
    #[test]
    fn a_windows_path_is_forward_slashed() {
        assert_eq!(
            sh_quoted(Path::new(r"C:\Program Files\spelunk\spelunk.exe")),
            "'C:/Program Files/spelunk/spelunk.exe'"
        );
    }

    /// A space in the path is why it is quoted at all.
    #[test]
    fn a_path_with_spaces_stays_one_word() {
        assert_eq!(
            sh_quoted(Path::new("/Users/a b/.local/bin/spelunk")),
            "'/Users/a b/.local/bin/spelunk'"
        );
    }

    /// A quote in the path would otherwise close the string and let the rest of
    /// the path parse as shell words.
    #[test]
    fn a_quote_in_the_path_cannot_escape_the_string() {
        assert_eq!(
            sh_quoted(Path::new("/home/o'brien/bin/spelunk")),
            r"'/home/o'\''brien/bin/spelunk'"
        );
    }

    /// The shim must carry a real path, never the literal placeholder: a hook
    /// reading `exec '{spelunk}'` would fail on every push.
    #[test]
    fn the_shim_embeds_a_resolved_absolute_path() {
        let body = pre_push_hook_body().expect("resolve current_exe");
        let exec = body
            .lines()
            .find(|l| l.starts_with("exec "))
            .expect("the shim execs the command");

        assert!(
            !body.contains("{spelunk}"),
            "placeholder left unsubstituted"
        );
        assert!(
            exec.contains("plumbing publish-notes --best-effort \"$@\""),
            "the shim must delegate every decision to the command: {exec}"
        );
        // `command -v` is withdrawn: it cannot occur (hooks are never cloned)
        // and it broke GUI clients, whose PATH comes from launchd.
        assert!(
            !body.contains("command -v"),
            "the shim must not look spelunk up on PATH: {body}"
        );

        let quoted = sh_quoted(&std::env::current_exe().unwrap());
        assert!(exec.contains(&quoted), "expected {quoted} in: {exec}");
        assert!(
            Path::new(quoted.trim_matches('\'')).is_absolute(),
            "the embedded path must be absolute: {quoted}"
        );
    }
}
