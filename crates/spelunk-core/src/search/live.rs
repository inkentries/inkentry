//! In-process structural ("ast-grep") fallback search.
//!
//! This is the zero-infrastructure fallback used by `spelunk search` /
//! `spelunk graph` when no index (or embedder) is available. It replaces the
//! previous `Command::new("ast-grep")` subprocess — structural matching now
//! runs inside the `spelunk` binary via `ast-grep-core`, so it works with no
//! external tool installed.
//!
//! Grammars are sourced from `ast-grep-language` (the same crate the indexer
//! uses for its tree-sitter parsers), so the fallback covers every one of the
//! 27 built-in languages structurally.

use ast_grep_core::{AstGrep, Pattern};
use ast_grep_language::{LanguageExt, SupportLang};
use ignore::WalkBuilder;
use std::path::Path;
use std::str::FromStr;

/// A single structural match, decoupled from any CLI output type.
///
/// Line numbers are **1-indexed** (tree-sitter/ast-grep positions are 0-indexed;
/// the conversion is applied here so callers get editor-style line numbers).
#[derive(Debug, Clone)]
pub struct LiveMatch {
    /// Path to the file containing the match (as walked, relative to `root`'s cwd).
    pub file_path: String,
    /// spelunk language name (e.g. "rust", "python").
    pub language: String,
    /// 1-indexed start line of the matched node.
    pub start_line: usize,
    /// 1-indexed end line of the matched node.
    pub end_line: usize,
    /// Source text of the matched node.
    pub text: String,
}

/// Map a spelunk language name to an `ast-grep-language` `SupportLang`.
///
/// Returns `None` for languages ast-grep-language does not ship (`proto`, `sql`)
/// or non-tree-sitter formats (`markdown`, `text`, …) — those are simply not
/// scanned by the structural fallback.
fn support_lang(spelunk_lang: &str) -> Option<SupportLang> {
    let name = match spelunk_lang {
        "javascript" | "jsx" => "javascript",
        "typescript" => "typescript",
        "tsx" => "tsx",
        // ast-grep-language's FromStr accepts the canonical names directly.
        other => other,
    };
    SupportLang::from_str(name).ok()
}

/// Detect an ast-grep `SupportLang` for a file by extension, limited to the
/// languages spelunk's indexer recognises (so fallback coverage matches the
/// indexer's language set plus the extra ast-grep-only grammars are reachable
/// through the same extension map).
fn detect_support_lang(path: &Path) -> Option<SupportLang> {
    let spelunk_lang = crate::indexer::parser::detect_language(path)?;
    support_lang(spelunk_lang)
}

/// Run a structural pattern search over the working tree rooted at `root`.
///
/// Walks files honouring `.gitignore` / `.ignore` (same traversal rules as the
/// indexer), parses each file whose language matches `pattern`'s intended
/// grammar, and collects up to `limit` matches. Patterns are compiled per
/// language on demand and cached for the duration of the call.
///
/// A malformed pattern for a given language is skipped for that language
/// (ast-grep patterns are language-specific); this mirrors the previous
/// subprocess behaviour where an unmatchable pattern simply yielded no results.
pub fn search_live_matches(pattern: &str, root: &Path, limit: usize) -> Vec<LiveMatch> {
    let mut out: Vec<LiveMatch> = Vec::new();
    // Compiled patterns are keyed by SupportLang; a pattern that fails to
    // compile for a language is recorded as `None` so we don't retry it.
    let mut compiled: std::collections::HashMap<SupportLang, Option<Pattern>> =
        std::collections::HashMap::new();

    for entry in WalkBuilder::new(root)
        .standard_filters(true)
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
    {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let Some(lang) = detect_support_lang(path) else {
            continue;
        };

        let pat = compiled
            .entry(lang)
            .or_insert_with(|| Pattern::try_new(pattern, lang).ok());
        let Some(pat) = pat.as_ref() else {
            continue;
        };

        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let spelunk_lang = crate::indexer::parser::detect_language(path).unwrap_or("unknown");
        let file_path = path.to_string_lossy().into_owned();

        let ast: AstGrep<_> = lang.ast_grep(&src);
        for m in ast.root().find_all(pat) {
            if out.len() >= limit {
                break;
            }
            let node = m.get_node();
            let start = node.start_pos().line() + 1;
            let end = node.end_pos().line() + 1;
            out.push(LiveMatch {
                file_path: file_path.clone(),
                language: spelunk_lang.to_string(),
                start_line: start,
                end_line: end,
                text: node.text().into_owned(),
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn finds_matches_in_process_without_external_binary() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n\
             fn caller() { greet(\"x\"); }\n",
        )
        .unwrap();

        // `greet($$$)` is the graph-fallback call pattern; it matches the call
        // site in `caller`, exercising the in-process structural matcher.
        let matches = search_live_matches("greet($$$ARGS)", dir.path(), 10);
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one match, got {matches:?}"
        );
        assert_eq!(matches[0].language, "rust");
        assert_eq!(matches[0].start_line, 2);
        assert!(matches[0].text.contains("greet"));
    }

    #[test]
    fn respects_limit() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn f() { g(); g(); g(); g(); }\n").unwrap();
        let matches = search_live_matches("g()", dir.path(), 2);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn multi_language_walk() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "def hello():\n    pass\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn hello() {}\n").unwrap();
        // Python pattern only matches the .py file; the .rs file is parsed with
        // the Rust grammar where this pattern does not compile/match.
        let matches = search_live_matches("def hello():\n    $$$BODY", dir.path(), 10);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].language, "python");
    }
}
