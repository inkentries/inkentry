#![no_main]

use libfuzzer_sys::fuzz_target;
use inkentry_core::indexer::parser::SourceParser;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };

    // Kept in sync with SUPPORTED_LANGUAGES in src/indexer/parser/mod.rs.
    let languages = &[
        "rust", "python", "javascript", "jsx", "typescript", "tsx",
        "go", "java", "c", "cpp", "json", "html", "css", "hcl",
        "sql", "proto", "markdown", "text", "notebook",
    ];
    let lang_idx = data.first().copied().unwrap_or(0) as usize % languages.len();
    let language = languages[lang_idx];

    let _ = SourceParser::parse(s, "fuzz_input", language);
});
