//! End-to-end coverage for the `search --mode text` empty-index guidance.
//!
//! On an initialized-but-empty project (`.inkentry/index.db` exists but holds
//! zero chunks), an explicit `--mode text` search previously emitted only the
//! shared `EmptyIndex` message ("index is empty — run `inkentry index` first"),
//! which demands an index the user may not want. The fix points `--mode text`
//! at the zero-setup modes that need no index (ast-grep, or omitting `--mode`).
//!
//! `--mode semantic`/`--mode hybrid` are deliberately left on the original
//! shared message: those modes genuinely need an index (plus an embedder), so
//! redirecting them to ast-grep would be misleading. These tests pin both the
//! new text-mode copy AND that the change is scoped to text mode.
//!
//! Driven through the real CLI (not the pure message selector) so a wiring
//! regression — e.g. the text branch being reordered after the shared return,
//! or the `mode == "text"` guard drifting — is caught end to end.

mod plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use std::path::Path;
use tempfile::TempDir;

/// The em-dash that must never appear in the new `--mode text` copy: user-facing
/// inkentry output follows the no-em-dash house rule. Its absence is a guard on
/// the copy itself, independent of the wording assertions below.
const EM_DASH: &str = "—";

/// Build an initialized-but-empty project: `<proj>/.inkentry/index.db` exists but
/// contains zero chunks (`chunk_count == 0`).
///
/// Indexing a directory with no indexable source files still runs migrations and
/// writes the project DB, so the DB exists (the resolver's existence check
/// passes) while the stats report zero chunks — exactly the state the empty-index
/// branch keys off. Offline (`INKENTRY_NO_SERVER=1`): with no chunks there is
/// nothing to embed, so no server is needed.
fn init_empty_project(home: &Path, proj: &Path) {
    inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj)
        .args(["index", "."])
        .assert()
        .success();

    assert!(
        proj.join(".inkentry").join("index.db").exists(),
        "indexing an empty dir must still create the project index.db"
    );
}

/// (a) `--mode text` on an empty index fails and redirects to the zero-setup
/// modes, with no em-dash in the copy.
#[test]
fn search_mode_text_empty_index_points_at_zero_setup_modes() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_empty_project(home.path(), proj.path());

    let assert = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["search", "anything", "--mode", "text"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains("no FTS index"),
        "text-mode empty-index error must name the missing FTS index; got: {stderr}"
    );
    assert!(
        stderr.contains("ast-grep"),
        "must offer the zero-setup ast-grep mode; got: {stderr}"
    );
    assert!(
        stderr.contains("omit --mode"),
        "must offer omitting --mode (auto) as a zero-setup path; got: {stderr}"
    );
    assert!(
        !stderr.contains(EM_DASH),
        "user-facing copy must not contain an em-dash; got: {stderr}"
    );
}

/// (b) Regression guard: `--mode semantic` on the SAME empty index still emits
/// the original shared `index is empty` message — the fix is scoped to text mode.
#[test]
fn search_mode_semantic_empty_index_keeps_shared_message() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_empty_project(home.path(), proj.path());

    let assert = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["search", "anything", "--mode", "semantic"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("index is empty"),
        "semantic mode must keep the original EmptyIndex message; got: {stderr}"
    );
    // The text-mode redirect copy must NOT leak into the semantic path.
    assert!(
        !stderr.contains("no FTS index"),
        "semantic mode must not use the text-mode redirect copy; got: {stderr}"
    );
}

/// (b, cont.) `--mode hybrid` shares the semantic path's index requirement, so it
/// too keeps the shared message. Covered independently since it is a distinct
/// mode string reaching the same branch.
#[test]
fn search_mode_hybrid_empty_index_keeps_shared_message() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_empty_project(home.path(), proj.path());

    let assert = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["search", "anything", "--mode", "hybrid"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("index is empty"),
        "hybrid mode must keep the original EmptyIndex message; got: {stderr}"
    );
    assert!(
        !stderr.contains("no FTS index"),
        "hybrid mode must not use the text-mode redirect copy; got: {stderr}"
    );
}
