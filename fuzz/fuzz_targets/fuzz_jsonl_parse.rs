#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    for line in s.lines() {
        let _ = serde_json::from_str::<serde_json::Value>(line);
    }
});
