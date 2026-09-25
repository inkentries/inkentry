// Rust keeps unit tests in the file they test, so a path cannot tell them
// apart; the attributes can. Chunks inside them are marked and not embedded.

use inkentry_core::indexer::{Chunk, SourceParser};

fn named<'a>(chunks: &'a [Chunk], name: &str) -> &'a Chunk {
    chunks
        .iter()
        .find(|c| c.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no chunk named {name:?}: {chunks:#?}"))
}

// Enough tests to push the module past the chunk token cap, so it is split
// into its members and gap windows rather than kept as one chunk.
fn test_fns() -> String {
    (0..40)
        .map(|i| {
            format!(
                "    #[test]\n    fn parses_case_{i}() {{\n        assert_eq!(parse(\"{i}\"), Some({i}));\n    }}\n\n"
            )
        })
        .collect()
}

#[test]
fn a_cfg_test_module_marks_its_members_and_windows_but_not_the_code_it_tests() {
    let src = format!(
        "/// Parses a number.\npub fn parse(s: &str) -> Option<u32> {{\n    s.parse().ok()\n}}\n\n\
         #[cfg(test)]\nmod tests {{\n    use super::parse;\n    use std::collections::HashMap;\n\n{}}}\n\n\
         pub fn after_the_tests() -> u32 {{\n    parse(\"1\").unwrap_or_default()\n}}\n",
        test_fns()
    );
    let chunks = SourceParser::parse(&src, "src/parse.rs", "rust").unwrap();

    assert!(!named(&chunks, "parse").in_test_code);
    assert!(!named(&chunks, "after_the_tests").in_test_code);
    assert!(named(&chunks, "parses_case_0").in_test_code);
    let imports = chunks
        .iter()
        .find(|c| c.content.contains("use std::collections::HashMap"))
        .expect("the module's imports are windowed");
    assert!(imports.in_test_code, "{imports:#?}");
}

#[test]
fn a_small_test_module_is_one_marked_chunk() {
    let src = "pub fn parse() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn parses() {\n        super::parse();\n    }\n}\n";
    let chunks = SourceParser::parse(src, "src/parse.rs", "rust").unwrap();

    assert!(!named(&chunks, "parse").in_test_code);
    assert!(named(&chunks, "tests").in_test_code);
    assert!(named(&chunks, "parses").in_test_code);
}

#[test]
fn test_runner_attributes_mark_a_function_outside_a_test_module() {
    let src = "#[tokio::test]\nasync fn fetches() {\n    run().await;\n}\n\n\
               #[test]\n// a comment between the attribute and the item\nfn parses() {\n    run();\n}\n\n\
               #[cfg(all(test, feature = \"slow\"))]\nfn slow_case() {\n    run();\n}\n\n\
               #[cfg(feature = \"slow\")]\nfn slow_path() {\n    run();\n}\n";
    let chunks = SourceParser::parse(src, "src/lib.rs", "rust").unwrap();

    assert!(named(&chunks, "fetches").in_test_code);
    assert!(named(&chunks, "parses").in_test_code);
    assert!(named(&chunks, "slow_case").in_test_code);
    assert!(
        !named(&chunks, "slow_path").in_test_code,
        "a feature gate is not a test gate"
    );
}

#[test]
fn an_inner_cfg_test_attribute_marks_the_whole_file() {
    let src = "#![cfg(test)]\n\nuse super::*;\n\nfn fixture() -> u32 {\n    1\n}\n";
    let chunks = SourceParser::parse(src, "src/fixtures.rs", "rust").unwrap();

    assert!(named(&chunks, "fixture").in_test_code);
}

#[test]
fn other_languages_are_left_to_the_path_rule() {
    let src = "def test_parses():\n    assert parse('1') == 1\n";
    let chunks = SourceParser::parse(src, "app/parse.py", "python").unwrap();

    assert!(chunks.iter().all(|c| !c.in_test_code), "{chunks:#?}");
}
