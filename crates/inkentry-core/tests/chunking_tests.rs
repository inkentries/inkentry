// Consolidated chunking/tokenization test binary: groups the previously
// separate chunker, language-support, and token-budget test files into one
// integration test crate to cut per-binary link overhead.

#[path = "chunking_tests/adversarial_chunker.rs"]
mod adversarial_chunker;
#[path = "chunking_tests/gap_coverage.rs"]
mod gap_coverage;
#[path = "chunking_tests/gap_names.rs"]
mod gap_names;
#[path = "chunking_tests/lang_csharp_kotlin_swift.rs"]
mod lang_csharp_kotlin_swift;
#[path = "chunking_tests/lang_js_ts_function_bindings.rs"]
mod lang_js_ts_function_bindings;
#[path = "chunking_tests/lang_php_ruby.rs"]
mod lang_php_ruby;
#[path = "chunking_tests/prop_chunker.rs"]
mod prop_chunker;
#[path = "chunking_tests/prop_token_budget.rs"]
mod prop_token_budget;
#[path = "chunking_tests/unit_chunker.rs"]
mod unit_chunker;
