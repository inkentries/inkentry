#![no_main]

use libfuzzer_sys::fuzz_target;
use inkentry_core::indexer::chunker::sliding_window;

// Window/overlap budgets derive from the token cap internally, so the source
// text is the only fuzzed input; name/docstring/parent_scope are just copied
// onto each chunk and passed as None here.
fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let _ = sliding_window(s, "fuzz_input", "text", None, None, None);
});
