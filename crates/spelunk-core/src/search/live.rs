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

/// A query is a structural ast-grep pattern when it contains a metavariable
/// (`$X`, `$$FOO`, `$$$ARGS`). Plain strings have none and are matched by
/// substring rather than node-text equality (spelunk-oss^130).
fn is_structural_pattern(query: &str) -> bool {
    query.contains('$')
}

/// Zero-setup search entry used by the CLI `search` fallback.
///
/// Structural patterns (with metavariables) run through the ast-grep matcher
/// unchanged. A plain string is matched **case-insensitively**: first as a
/// substring of identifier/text (named-leaf) nodes, then — for any file the
/// node pass leaves uncovered — as a literal line scan, so a substring that
/// demonstrably exists in a file never returns empty (spelunk-oss^130).
pub fn search_live_query(query: &str, root: &Path, limit: usize) -> Vec<LiveMatch> {
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }
    if is_structural_pattern(query) {
        return search_live_matches(query, root, limit);
    }
    search_live_substring(query, root, limit)
}

/// Case-insensitive plain-string search: substring over identifier/text nodes,
/// with a literal line scan beneath ast-grep for files the node pass misses
/// (non-code files, or a substring spanning tokens / in an unparsed region).
fn search_live_substring(query: &str, root: &Path, limit: usize) -> Vec<LiveMatch> {
    let needle = query.to_lowercase();
    let mut out: Vec<LiveMatch> = Vec::new();

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
        let Ok(src) = std::fs::read_to_string(path) else {
            continue; // non-UTF8 / binary — skip, mirroring the structural path.
        };
        if !src.to_lowercase().contains(&needle) {
            continue;
        }
        let file_path = path.to_string_lossy().into_owned();
        let before = out.len();

        // Pass 1 — substring of identifier/text nodes (named leaves) via ast-grep.
        if let Some(lang) = detect_support_lang(path) {
            let spelunk_lang = crate::indexer::parser::detect_language(path).unwrap_or("unknown");
            let ast: AstGrep<_> = lang.ast_grep(&src);
            for node in ast.root().dfs() {
                if out.len() >= limit {
                    break;
                }
                // Named leaves are identifiers/comments/literals — the terminal
                // tokens; this skips punctuation and every enclosing ancestor.
                if !(node.is_named() && node.is_named_leaf()) {
                    continue;
                }
                let text = node.text();
                if !text.to_lowercase().contains(&needle) {
                    continue;
                }
                out.push(LiveMatch {
                    file_path: file_path.clone(),
                    language: spelunk_lang.to_string(),
                    start_line: node.start_pos().line() + 1,
                    end_line: node.end_pos().line() + 1,
                    text: text.into_owned(),
                });
            }
        }

        // Pass 2 (beneath ast-grep) — the file contains the substring but the
        // node pass matched nothing here. A literal line scan guarantees the
        // substring is never silently dropped.
        if out.len() == before {
            let spelunk_lang = crate::indexer::parser::detect_language(path).unwrap_or("text");
            for (i, line) in src.lines().enumerate() {
                if out.len() >= limit {
                    break;
                }
                if line.to_lowercase().contains(&needle) {
                    out.push(LiveMatch {
                        file_path: file_path.clone(),
                        language: spelunk_lang.to_string(),
                        start_line: i + 1,
                        end_line: i + 1,
                        text: line.to_string(),
                    });
                }
            }
        }
    }

    out
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

    // ── plain-string substring search (spelunk-oss^130) ─────────────────────────

    #[test]
    fn plain_substring_matches_identifier() {
        // The reported bug: a substring of an identifier found nothing.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("m.rs"),
            "struct BillingEntity { id: u64 }\n",
        )
        .unwrap();
        let matches = search_live_query("Billing", dir.path(), 10);
        assert!(
            matches.iter().any(|m| m.text.contains("BillingEntity")),
            "substring 'Billing' should match the BillingEntity identifier: {matches:?}"
        );
    }

    #[test]
    fn plain_substring_is_case_insensitive() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("m.rs"), "struct BillingEntity;\n").unwrap();
        let matches = search_live_query("billing", dir.path(), 10);
        assert!(
            !matches.is_empty(),
            "lowercase 'billing' should match BillingEntity"
        );
    }

    #[test]
    fn plain_exact_identifier_still_matches() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("m.rs"),
            "fn f() { let _ = BillingEntity; }\n",
        )
        .unwrap();
        let matches = search_live_query("BillingEntity", dir.path(), 10);
        assert!(
            matches.iter().any(|m| m.text.contains("BillingEntity")),
            "exact identifier must still match: {matches:?}"
        );
    }

    #[test]
    fn plain_absent_string_returns_nothing() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("m.rs"), "struct BillingEntity;\n").unwrap();
        let matches = search_live_query("Zzznotpresent", dir.path(), 10);
        assert!(matches.is_empty(), "absent string must yield no matches");
    }

    #[test]
    fn plain_substring_matches_non_code_file() {
        // Exercises the literal line-scan pass beneath ast-grep: a `.txt` file has
        // no tree-sitter grammar, so the node pass is skipped entirely.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "the BillingEntity ledger\n").unwrap();
        let matches = search_live_query("Billing", dir.path(), 10);
        assert!(
            !matches.is_empty(),
            "literal text pass should find the substring in a non-code file"
        );
    }

    #[test]
    fn structural_pattern_stays_structural() {
        // `$X.foo()` contains a metavariable, so it must match the method call
        // structurally — not substring-match the `.foo()` text in the string
        // literal on the same line.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("m.rs"),
            "fn f(a: T) { a.foo(); let _s = \"call .foo() somewhere\"; }\n",
        )
        .unwrap();
        let matches = search_live_query("$X.foo()", dir.path(), 10);
        assert_eq!(
            matches.len(),
            1,
            "structural pattern matches the call only, not the string literal: {matches:?}"
        );
        assert!(matches[0].text.contains("a.foo()"));
    }
}
