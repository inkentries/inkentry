// Parks the process at a named crash point so the crash-safety suite can SIGKILL
// it inside an exact write window instead of racing sleeps. It prints a marker,
// then blocks on stdin until killed or released by a written byte.
//
// Gated on `debug_assertions`, not `cfg(test)`: the harness spawns the real
// binary, which never gets `cfg(test)`. Release builds carry no reachable code.
#[cfg(debug_assertions)]
pub(super) fn pause_at(point: &str, subject: &str) {
    let Ok(target) = std::env::var("INKENTRY_TEST_CRASH_POINT") else {
        return;
    };
    if target != format!("{point}:{subject}") {
        return;
    }
    println!("INKENTRY_TEST_CRASH_POINT_REACHED:{target}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut buf = [0u8; 1];
    let _ = std::io::Read::read(&mut std::io::stdin(), &mut buf);
}

#[cfg(not(debug_assertions))]
pub(super) fn pause_at(_point: &str, _subject: &str) {}
