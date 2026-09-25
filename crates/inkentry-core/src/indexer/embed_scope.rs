//! Which chunks get a vector, and which are left to full-text search alone
//! (ADR-104).
//!
//! Embedding is the slow part of indexing, and on a real repository most of
//! the tokens it spends buy little recall: test code is found by the names it
//! exercises, changelog entries by the words they share with the query, and a
//! window of unnamed code between two definitions has no title to embed under.
//! These chunks stay in the full-text index, where a lexical match still finds
//! them, and the hybrid ranking fuses them in from that side only.

/// Languages whose unnamed windows are prose or data rather than code left
/// between named definitions, so they keep their vector.
const PROSE_LANGUAGES: &[&str] = &["markdown", "text", "docx", "pdf", "spreadsheet", "notebook"];

/// Directory names that hold tests in the common layouts: `tests/` (Rust,
/// Python), `test/` and `spec/` (Ruby, Java's `src/test`), `__tests__/` and
/// `__mocks__/` (Jest), `testdata/` (Go).
const TEST_DIRS: &[&str] = &[
    "test",
    "tests",
    "spec",
    "specs",
    "__tests__",
    "__mocks__",
    "testdata",
    "e2e",
];

/// Whether a chunk is kept out of the embed queue. Evaluated on stored columns
/// (`files.path`, `files.language`, `chunks.node_type`, `chunks.name`), so the
/// schema step that introduced the flag computes it for existing rows exactly
/// as a fresh index does.
pub fn is_text_only(path: &str, language: &str, node_type: &str, name: Option<&str>) -> bool {
    language == "json"
        || is_test_path(path)
        || is_changelog(path, language)
        || (node_type == "verbatim" && name.is_none() && !PROSE_LANGUAGES.contains(&language))
}

fn is_test_path(path: &str) -> bool {
    let mut components = path.split(['/', '\\']);
    let Some(file) = components.next_back() else {
        return false;
    };
    if components.any(|dir| TEST_DIRS.contains(&dir)) {
        return true;
    }
    let (stem, ext) = file.rsplit_once('.').unwrap_or((file, ""));
    let lower = stem.to_ascii_lowercase();
    lower == "test"
        || lower == "tests"
        || (ext == "py" && lower.starts_with("test_"))
        || [".test", ".spec", "_test", "_tests", "_spec"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
        || has_camel_test_suffix(stem)
}

/// `FooTest`, `FooTests`, `FooSpec` (Java, Kotlin, C#, Swift). The capital must
/// start a new word, so `Contest` and `Latest` are not tests.
fn has_camel_test_suffix(stem: &str) -> bool {
    ["Test", "Tests", "Spec"].iter().any(|suffix| {
        stem.strip_suffix(suffix)
            .and_then(|head| head.chars().last())
            .is_some_and(|c| c.is_lowercase() || c.is_ascii_digit())
    })
}

fn is_changelog(path: &str, language: &str) -> bool {
    if !matches!(language, "markdown" | "text") {
        return false;
    }
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let lower = file.to_ascii_lowercase();
    ["changelog", "changes", "history"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::is_text_only;

    fn code(path: &str) -> bool {
        is_text_only(path, "rust", "function", Some("f"))
    }

    #[test]
    fn test_files_are_recognised_across_the_common_layouts() {
        for path in [
            "crates/core/tests/search.rs",
            "src/storage/memory/tests.rs",
            "spec/models/invoice_spec.rb",
            "test/models/invoice_test.rb",
            "pkg/billing/invoice_test.go",
            "app/tests/test_views.py",
            "front/src/components/__tests__/Invoice.tsx",
            "front/src/utils/format.test.ts",
            "front/src/utils/format.spec.tsx",
            "src/test/java/com/acme/InvoiceTest.java",
            "Sources/AppTests/InvoiceTests.swift",
            "internal/parse/testdata/sample.go",
        ] {
            assert!(code(path), "{path} is a test file");
        }
    }

    #[test]
    fn product_code_whose_name_merely_contains_test_is_embedded() {
        for path in [
            "src/contest.rs",
            "src/Latest.java",
            "src/attestation.go",
            "app/models/test_run.rb",
            "src/testing_helpers.rs",
            "front/src/specification.ts",
        ] {
            assert!(!code(path), "{path} is not a test file");
        }
    }

    #[test]
    fn changelogs_are_text_only_but_a_history_module_is_not() {
        assert!(is_text_only(
            "CHANGELOG.md",
            "markdown",
            "section",
            Some("1.0")
        ));
        assert!(is_text_only(
            "docs/CHANGELOG-v0.md",
            "markdown",
            "section",
            Some("0.9")
        ));
        assert!(is_text_only("HISTORY.txt", "text", "verbatim", None));
        assert!(!is_text_only(
            "src/history.rs",
            "rust",
            "function",
            Some("undo")
        ));
    }

    #[test]
    fn json_is_text_only() {
        assert!(is_text_only(
            "config/settings.json",
            "json",
            "verbatim",
            None
        ));
    }

    #[test]
    fn an_unnamed_window_of_code_is_text_only_but_a_named_one_is_not() {
        assert!(is_text_only(
            "app/models/invoice.rb",
            "ruby",
            "verbatim",
            None
        ));
        assert!(!is_text_only(
            "src/big.rs",
            "rust",
            "verbatim",
            Some("oversized_function")
        ));
    }

    #[test]
    fn an_unnamed_window_of_prose_keeps_its_vector() {
        assert!(!is_text_only("NOTES.txt", "text", "verbatim", None));
        assert!(!is_text_only("docs/intro.md", "markdown", "verbatim", None));
    }

    #[test]
    fn named_definitions_and_doc_sections_are_embedded() {
        assert!(!is_text_only(
            "src/lib.rs",
            "rust",
            "function",
            Some("parse")
        ));
        assert!(!is_text_only(
            "docs/guide.md",
            "markdown",
            "section",
            Some("Install")
        ));
    }
}
