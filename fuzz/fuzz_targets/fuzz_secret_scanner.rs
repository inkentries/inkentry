#![no_main]

use libfuzzer_sys::fuzz_target;
use inkentry_core::indexer::secrets::contains_secret;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let _ = contains_secret(s);
});
