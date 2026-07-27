// Deterministic crash-point synchronisation for the crash-safety integration
// suite (`crates/spelunk-cli/tests/crash_safety.rs`): landing a real SIGKILL
// inside a specific write window by racing wall-clock sleeps against another
// process is inherently flaky, so the harness instead waits for this
// process to print a marker proving it reached the exact window, then kills
// it while it is parked here. Reading from a pipe the harness never writes
// to blocks until the harness closes it (by killing us) or writes a byte (to
// release us without a crash, used by tests that need a held write window
// rather than a kill). A no-op for every real invocation: the env var this
// checks is never set outside the test harness.
pub(super) fn pause_at(point: &str, subject: &str) {
    let Ok(target) = std::env::var("SPELUNK_TEST_CRASH_POINT") else {
        return;
    };
    if target != format!("{point}:{subject}") {
        return;
    }
    println!("SPELUNK_TEST_CRASH_POINT_REACHED:{target}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut buf = [0u8; 1];
    let _ = std::io::Read::read(&mut std::io::stdin(), &mut buf);
}
