#![no_main]

use libfuzzer_sys::fuzz_target;
use serde::Deserialize;
use std::collections::HashMap;

// ClaudeHistoryEntry and PastedContent are private in
// src/cli/cmd/memory/harvest_claude.rs, so they're replicated here to fuzz
// serde deserialization.
#[derive(Deserialize)]
struct PastedContent {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct ClaudeHistoryEntry {
    #[serde(default)]
    display: String,
    #[serde(rename = "pastedContents", default)]
    pasted_contents: HashMap<String, PastedContent>,
    #[serde(default)]
    timestamp: i64,
    #[serde(default)]
    project: String,
    #[serde(rename = "sessionId", default)]
    session_id: String,
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let _ = serde_json::from_str::<ClaudeHistoryEntry>(s);
});
