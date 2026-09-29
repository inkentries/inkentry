#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    // Mirrors the private `escape_xml` in src/cli/cmd/ask.rs exactly.
    let _ = s.replace('<', "&lt;").replace('>', "&gt;");
});
