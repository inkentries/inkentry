// Git-isolation fixture shared by every crate's tests. Gated like
// `config::secret_store::MemoryStore`: this crate's own `#[cfg(test)]` unit
// tests get it for free, and a downstream crate's tests (or this crate's own
// `tests/` integration binaries, via a self-referencing dev-dependency) reach
// it by enabling the `test-support` feature.

use std::path::Path;
use std::sync::Once;

// Process-wide, not per-Command: a helper that only sets env on the Command
// it builds itself never reaches git spawned by the code under test.
//
// A temp repo's local config does not shadow an ambient global value it
// never sets itself: a global `notes.rewriteRef` reads back as
// already-covered, or a global `core.hooksPath` fires a foreign hook on a
// setup commit.
//
// `/dev/null` isn't a Windows path, but git skips a scope whenever its var is
// set, whatever the path resolves to, so this isolates Windows too.
//
// `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` only redirect config files, not
// `GIT_AUTHOR_*`/`GIT_COMMITTER_*`/`EMAIL`, which git reads before config and
// so override an explicit `git config user.name`/`user.email` if the ambient
// process happens to export them. Those are cleared too.
pub fn isolate_git_config() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: every caller here calls this first and `Once` blocks the
        // rest until it returns, so no thread can be spawning git (reading
        // environ) while these run.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
            for var in [
                "GIT_AUTHOR_NAME",
                "GIT_AUTHOR_EMAIL",
                "GIT_AUTHOR_DATE",
                "GIT_COMMITTER_NAME",
                "GIT_COMMITTER_EMAIL",
                "GIT_COMMITTER_DATE",
                "EMAIL",
            ] {
                std::env::remove_var(var);
            }
        }
    });
}

// The sanctioned way to spawn git in a test: always isolates config first,
// so a caller can't forget the separate setup step.
// `scripts/check-git-isolation.sh` enforces in CI that a test file spawning
// git wires this in.
pub fn git_command(cwd: &Path) -> std::process::Command {
    isolate_git_config();
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(cwd);
    cmd
}
