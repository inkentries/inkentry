//! Shared test helpers.
#![allow(dead_code)]
//!
//! Import with `mod common;` or `use crate::common::*;` inside integration tests.

use std::sync::OnceLock;

/// Register the sqlite-vec extension exactly once for the test process.
///
/// sqlite3_auto_extension is process-global; calling it more than once per
/// address is a no-op but calling it from multiple threads without
/// synchronisation is UB.  `OnceLock` guarantees single initialisation.
///
/// Tests that open a `Database` **must** call this first.
/// Annotate those tests with `#[serial_test::serial]` so the global
/// registration happens before any connection is opened.
pub fn register_sqlite_vec() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Open an in-memory `spelunk_core::storage::Database` for tests.
///
/// Calls `register_sqlite_vec()` automatically.
pub fn open_test_db() -> spelunk_core::storage::Database {
    register_sqlite_vec();
    spelunk_core::storage::Database::open(std::path::Path::new(":memory:"))
        .expect("failed to open in-memory database")
}

/// Drop the machine's global/system git config for every git this process
/// spawns, including one the code under test spawns itself. Must be
/// process-wide, not per-`Command`: a helper that only sets env on the
/// `Command` it builds itself never reaches git spawned by the code under
/// test.
///
/// A temp repo's local config does not shadow an ambient value the repo
/// never sets: a global `notes.rewriteRef` reads back as already-covered, or
/// a global `core.hooksPath` (husky, lefthook, the pre-commit framework)
/// fires a foreign hook on a setup commit.
///
/// `/dev/null` is not a Windows path, but git skips a scope whenever its var
/// is set, whatever the path resolves to, so this isolates on Windows too.
pub fn isolate_git_config() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: every git-touching helper here calls this first and
        // `Once` blocks the rest until it returns, so no thread can be
        // spawning git (reading environ) while these run.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        }
    });
}

/// Build a `git` `Command` rooted at `cwd`, isolated from the developer's
/// ambient global/system git config.
///
/// This is the sanctioned way for a `spelunk-core` integration test to spawn
/// `git`: it always calls [`isolate_git_config`] first, so a caller cannot
/// construct an un-isolated one by forgetting a separate setup step.
/// `scripts/check-git-isolation.sh` enforces in CI that a test file spawning
/// `git` wires this module in.
pub fn git_command(cwd: &std::path::Path) -> std::process::Command {
    isolate_git_config();
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(cwd);
    cmd
}
