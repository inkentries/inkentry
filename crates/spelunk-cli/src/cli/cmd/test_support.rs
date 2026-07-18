//! Shared helper for unit tests under `cli::cmd`.
//!
//! `tests/plumbing_helpers.rs` carries the same helper for the `tests/`
//! integration binaries, but a unit test compiled into `src/` cannot reach a
//! file under `tests/`, so this is the `src/`-side counterpart.

/// Drop the machine's global/system git config for every git this process
/// spawns: a setup git a test runs directly, and any git the code under
/// test spawns for itself. Must be process-wide, not per-`Command`: a
/// helper that only sets env on the `Command` it builds itself never
/// reaches a git spawned inside the function under test.
///
/// A temp repo's local config does not shadow an ambient value the repo
/// never sets, so an ambient `core.hooksPath` (husky, lefthook, the
/// pre-commit framework) fires a foreign hook inside a test's throwaway
/// repo, and an ambient `commit.gpgsign` signs as an identity no
/// contributor holds a key for.
///
/// `/dev/null` is not a Windows path, but git skips a scope whenever its
/// var is set, whatever the path resolves to, so this isolates on Windows
/// too.
pub(crate) fn isolate_git_config() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: every caller here calls this first and `Once` blocks the
        // rest until it returns, so no thread can be spawning git (reading
        // environ) while these run.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        }
    });
}
