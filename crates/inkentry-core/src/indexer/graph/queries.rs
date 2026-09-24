//! The vendored `locals.scm` scope queries, compiled once per language.
//!
//! The query files under `queries/` are upstream's, unmodified (provenance and
//! licence in `queries/README.md`). Upstream composes some of them with an
//! `; inherits:` header that its own loader resolves; the part lists below do
//! the same, so each file stays byte-for-byte what upstream ships.

use std::sync::OnceLock;

const RUST: &[&str] = &[include_str!("queries/rust/locals.scm")];
const PYTHON: &[&str] = &[include_str!("queries/python/locals.scm")];
const JAVASCRIPT: &[&str] = &[
    include_str!("queries/ecma/locals.scm"),
    include_str!("queries/javascript/locals.scm"),
];
const TYPESCRIPT: &[&str] = &[
    include_str!("queries/ecma/locals.scm"),
    include_str!("queries/typescript/locals.scm"),
];
const GO: &[&str] = &[include_str!("queries/go/locals.scm")];
const RUBY: &[&str] = &[include_str!("queries/ruby/locals.scm")];
const JAVA: &[&str] = &[include_str!("queries/java/locals.scm")];
const C: &[&str] = &[include_str!("queries/c/locals.scm")];
const CPP: &[&str] = &[
    include_str!("queries/c/locals.scm"),
    include_str!("queries/cpp/locals.scm"),
];
const CSHARP: &[&str] = &[include_str!("queries/c_sharp/locals.scm")];
const PHP: &[&str] = &[include_str!("queries/php_only/locals.scm")];
const KOTLIN: &[&str] = &[include_str!("queries/kotlin/locals.scm")];
const SWIFT: &[&str] = &[include_str!("queries/swift/locals.scm")];

/// Every language with a vendored query, in the grammar names the indexer
/// uses, paired with the query files that make it up.
const LANGUAGES: &[(&str, &[&str])] = &[
    ("rust", RUST),
    ("python", PYTHON),
    ("javascript", JAVASCRIPT),
    ("jsx", JAVASCRIPT),
    ("typescript", TYPESCRIPT),
    ("tsx", TYPESCRIPT),
    ("go", GO),
    ("ruby", RUBY),
    ("java", JAVA),
    ("c", C),
    ("cpp", CPP),
    ("csharp", CSHARP),
    ("php", PHP),
    ("kotlin", KOTLIN),
    ("swift", SWIFT),
];

fn compile(language: &str, parts: &[&str]) -> Result<tree_sitter::Query, String> {
    let grammar = crate::indexer::parser::ts_language_pub(language).map_err(|e| e.to_string())?;
    let mut query =
        tree_sitter::Query::new(&grammar, &parts.join("\n")).map_err(|e| e.to_string())?;
    // Resolution walks scopes from the callee node itself, so the per-token
    // reference captures (every identifier in the file) would be pure cost.
    query.disable_capture("local.reference");
    Ok(query)
}

/// The compiled locals query for `language`, or `None` for a language with
/// no vendored query. Each query compiles on first use, so an index of a
/// one-language tree compiles one. A query that fails to compile against its
/// grammar is also `None` (logged once), so indexing degrades to unresolved
/// edges instead of failing; the compile test below keeps that from shipping.
pub(super) fn locals_query(language: &str) -> Option<&'static tree_sitter::Query> {
    static COMPILED: [OnceLock<Option<tree_sitter::Query>>; LANGUAGES.len()] =
        [const { OnceLock::new() }; LANGUAGES.len()];
    let index = LANGUAGES.iter().position(|&(lang, _)| lang == language)?;
    let (lang, parts) = LANGUAGES[index];
    COMPILED[index]
        .get_or_init(|| {
            compile(lang, parts)
                .inspect_err(|e| tracing::warn!("locals query for {lang} does not compile: {e}"))
                .ok()
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::{LANGUAGES, compile, locals_query};

    #[test]
    fn every_vendored_query_compiles_against_the_grammar_it_is_used_with() {
        for &(lang, parts) in LANGUAGES {
            if let Err(e) = compile(lang, parts) {
                panic!("{lang}'s locals query does not compile: {e}");
            }
        }
    }

    #[test]
    fn a_language_without_a_vendored_query_has_none() {
        assert!(locals_query("html").is_none());
    }
}
