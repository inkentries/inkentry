// One chunking test binary, not several: groups the chunker and
// language-support test files to cut per-binary link overhead.

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
#[path = "chunking_tests/ruby_class_body.rs"]
mod ruby_class_body;
#[path = "chunking_tests/rust_test_code.rs"]
mod rust_test_code;
#[path = "chunking_tests/unit_chunker.rs"]
mod unit_chunker;
