// Every test that runs the `inkentry` binary must disable loopback
// auto-discovery's fixed-port fallback.
//
// Discovery step 3b (`capability/probe.rs`) probes a fixed port. On a
// developer's machine that is their own daemon, so an unisolated test does not
// run against nothing — it runs against their server and sends it real
// embedding work. Neither guard that should catch this can: `egress_containment`
// permits loopback by construction, and CI has nothing listening, so the suite
// is green in the only environment where the bug cannot happen (inkentry-oss^5).
//
// Pointing `INKENTRY_STATE_DIR` at an empty dir does NOT isolate: that defeats
// step 3a only, and 3b is the step that reaches off the test's world.
//
// `plumbing_helpers::inkentry_bin_in` sets the override for every command built
// through it. Tests in other crates cannot use that helper, so the invariant
// this pins is per-file: a file that launches the binary must also set
// `INKENTRY_TEST_DISCOVERY_PORT`.

use std::path::{Path, PathBuf};

// Ways a test can launch the binary.
// `CARGO_BIN_EXE_inkentry"` with the closing quote: the server binary's
// variable shares this prefix and does not auto-discover anything.
const LAUNCHERS: [&str; 2] = ["cargo_bin(", r#"CARGO_BIN_EXE_inkentry""#];

// Either disables step 3b: the override skips the fallback, and
// INKENTRY_NO_SERVER short-circuits discovery before any probe at all.
const OVERRIDE: &str = "INKENTRY_TEST_DISCOVERY_PORT";
const KILL_SWITCH: &str = "INKENTRY_NO_SERVER";

// plumbing_helpers.rs defines the isolation; this file names both sides to
// describe them, so each would match its own guard.
const EXEMPT: [&str; 2] = ["plumbing_helpers.rs", "loopback_isolation.rs"];

fn test_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(dir).expect("reading a test directory") {
        let path = entry.expect("reading a directory entry").path();
        if path.is_dir() {
            test_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

// Every crate's tests, not just this one: the server crate drives the CLI too.
fn all_test_files() -> Vec<PathBuf> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ directory");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates).expect("reading crates/") {
        let dir = entry
            .expect("reading a crate directory")
            .path()
            .join("tests");
        test_sources(&dir, &mut files);
    }
    files
}

#[test]
fn every_test_that_launches_the_binary_disables_the_discovery_fallback() {
    let files = all_test_files();
    assert!(!files.is_empty(), "found no test sources to scan");

    let mut offenders = Vec::new();
    for file in &files {
        if file
            .file_name()
            .is_some_and(|n| EXEMPT.iter().any(|e| n == *e))
        {
            continue;
        }
        let text = std::fs::read_to_string(file).expect("reading a test source file");
        // Files that go through `plumbing_helpers::inkentry_bin` carry no
        // launcher of their own, so they never reach this check. Only a file
        // launching the binary itself has to prove it isolates — and it proves
        // it by setting the variable, not by mentioning the helper in a doc
        // comment.
        let launches = LAUNCHERS.iter().any(|l| text.contains(l));
        let isolated = text.contains(OVERRIDE) || text.contains(KILL_SWITCH);
        if launches && !isolated {
            offenders.push(file.display().to_string());
        }
    }

    assert!(
        offenders.is_empty(),
        "these run the `inkentry` binary without disabling loopback discovery's \
         fixed-port fallback, so a test run reaches the developer's own daemon. \
         Build the command with `plumbing_helpers::inkentry_bin`, or set \
         `{OVERRIDE}=0` (or `{KILL_SWITCH}=1`) on it:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_shared_helper_sets_the_override() {
    // The per-file guard accepts `inkentry_bin` as proof of isolation, which
    // only holds while the helper actually sets it.
    let helper = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("plumbing_helpers.rs");
    let text = std::fs::read_to_string(&helper).expect("reading the spawn chokepoint");

    assert!(
        text.contains(&format!(r#".env("{OVERRIDE}", "0")"#)),
        "plumbing_helpers.rs must set {OVERRIDE}=0 on every command it builds"
    );
}
