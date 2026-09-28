//! Deterministic structural chunk summaries for the built-in tier.
//!
//! The `summary:` slot in [`Chunk::embedding_text`] bridges retrieval
//! vocabulary — natural-language words a query would use, sitting next to the
//! code they point at — composed here from signals already present after
//! parse, with no model, key or network involved. The composition is
//! **byte-identical** for the same chunk and edges on every run: an
//! interrupted re-embed must not produce a different vector for the same
//! input.
//!
//! [`Chunk::embedding_text`]: crate::indexer::Chunk::embedding_text

use std::collections::HashSet;

use crate::search::tokens::estimate_tokens;

/// Provenance tag for the embedding-input composition scheme, stamped into a
/// DB's `index_meta` alongside `embedding_model` and `chunker_config`. Bump
/// this whenever a change to the composed `summary:` text (including the
/// tier-3 MMR `λ`, folded into this one scheme) invalidates stored vectors.
pub const SUMMARY_SCHEME: &str = "structural_v1";

/// Hard cap on the composed `summary:` slot, in estimated tokens — the same
/// order of magnitude as the one-sentence LLM summary it replaces, never
/// several times it. Bounds a long summary displacing the code tail at the
/// embedder's context boundary, and a bloated summary diluting the pooled
/// vector toward prose and away from code.
pub const SUMMARY_TOKEN_CAP: usize = 96;

// Bounds on salient-literal extraction so a crafted/generated chunk can't drive
// unbounded work.
const MAX_SALIENT_LITERALS: usize = 6;
const MAX_LITERAL_SCAN_CHARS: usize = 64 * 1024;

/// Compose the structural `summary:` slot for a **named** chunk.
///
/// Ingredients are assembled in a fixed priority order and appended until
/// [`SUMMARY_TOKEN_CAP`] is reached; the first ingredient that would overflow
/// is dropped **whole** (never truncated mid-ingredient), as are all
/// lower-priority ones:
///
/// 1. docstring, first sentence — human-written intent, highest signal;
/// 2. split symbol name (`retry_with_backoff` → "retry with backoff");
/// 3. split callee names, in the graph's deterministic order;
/// 4. salient literals — error/log strings, last (noisiest, secret-bearing).
///
/// `callees` must already be in deterministic order; the caller supplies the
/// graph's SQL `ORDER BY target_name`, never hash-iteration order.
pub fn compose_structural_summary(
    name: &str,
    docstring: Option<&str>,
    callees: &[String],
    content: &str,
) -> String {
    let ingredients = [
        docstring.map(first_sentence).unwrap_or_default(),
        split_identifier(name),
        split_callees(callees),
        salient_literals(content),
    ];
    assemble(&ingredients, SUMMARY_TOKEN_CAP)
}

// Empty ingredients are skipped, not terminal, so a missing docstring never
// suppresses the split name below it.
fn assemble(ingredients: &[String], cap: usize) -> String {
    let mut composed = String::new();
    for ing in ingredients {
        if ing.is_empty() {
            continue;
        }
        let candidate = if composed.is_empty() {
            ing.clone()
        } else {
            format!("{composed} {ing}")
        };
        if estimate_tokens(&candidate) > cap {
            break;
        }
        composed = candidate;
    }
    composed
}

// A sentence ends at the first `.`/`!`/`?` followed by whitespace or
// end-of-string; a docstring with no such terminator yields the whole
// (whitespace-collapsed) text.
fn first_sentence(doc: &str) -> String {
    let text = collapse_whitespace(doc);
    if text.is_empty() {
        return String::new();
    }
    let mut end = text.len();
    for (i, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            let after = i + ch.len_utf8();
            let next_is_boundary = text[after..].chars().next().is_none_or(char::is_whitespace);
            if next_is_boundary {
                end = after;
                break;
            }
        }
    }
    text[..end].to_string()
}

// Splits on `snake_case`, `kebab-case`, path (`::`, `.`) and
// `camelCase`/`PascalCase` boundaries: `retry_with_backoff` -> "retry with
// backoff"; `EdgeExtractor` -> "edge extractor".
fn split_identifier(ident: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for ch in ident.chars() {
        if ch == '_' || ch == '-' || ch == ':' || ch == '.' || ch.is_whitespace() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else if ch.is_uppercase() {
            if matches!(prev, Some(p) if p.is_lowercase() || p.is_ascii_digit()) && !cur.is_empty()
            {
                words.push(std::mem::take(&mut cur));
            }
            cur.extend(ch.to_lowercase());
        } else {
            cur.push(ch);
        }
        prev = Some(ch);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words.join(" ")
}

// Dedups words while preserving first-occurrence order; the set is for
// membership only, never for output ordering.
fn split_callees(callees: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for callee in callees {
        for word in split_identifier(callee).split(' ') {
            if !word.is_empty() && seen.insert(word.to_string()) {
                out.push(word.to_string());
            }
        }
    }
    out.join(" ")
}

// Message-like double-quoted literals (containing a space) that a
// natural-language query might echo; last because they're the noisiest and
// the most likely to carry a secret (the caller secret-scans the composed
// summary before storing it).
fn salient_literals(content: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cur = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in content.chars().take(MAX_LITERAL_SCAN_CHARS) {
        if in_string {
            if escaped {
                escaped = false;
                cur.push(ch);
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                let lit = cur.trim();
                if lit.contains(' ') && lit.chars().count() >= 3 && seen.insert(lit.to_string()) {
                    out.push(lit.to_string());
                    if out.len() >= MAX_SALIENT_LITERALS {
                        break;
                    }
                }
                cur.clear();
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            in_string = true;
            cur.clear();
        }
    }
    out.join(" ")
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_ingredients_in_priority_order() {
        let summary = compose_structural_summary(
            "retry_with_backoff",
            Some("Retries an operation with exponential backoff."),
            &["connect".to_string(), "sleep".to_string()],
            r#"fn f() { log::warn!("giving up after retries"); }"#,
        );
        let doc_at = summary
            .find("Retries an operation")
            .expect("docstring first");
        let name_at = summary
            .find("retry with backoff")
            .expect("split name second");
        let callee_at = summary.find("connect sleep").expect("split callees third");
        let lit_at = summary
            .find("giving up after retries")
            .expect("salient literal last");
        assert!(
            doc_at < name_at && name_at < callee_at && callee_at < lit_at,
            "ingredients out of priority order: {summary:?}"
        );
    }

    #[test]
    fn never_exceeds_the_token_cap() {
        let docstring = "word ".repeat(500);
        let callees: Vec<String> = (0..500).map(|i| format!("callee_number_{i}")).collect();
        let content: String = (0..500)
            .map(|i| format!("log(\"message number {i} here\");"))
            .collect();
        let summary = compose_structural_summary(
            "a_very_long_symbol_name_that_keeps_going_and_going",
            Some(&docstring),
            &callees,
            &content,
        );
        assert!(
            estimate_tokens(&summary) <= SUMMARY_TOKEN_CAP,
            "composed summary is {} tokens, over the {SUMMARY_TOKEN_CAP} cap",
            estimate_tokens(&summary)
        );
    }

    #[test]
    fn overflow_drops_lower_priority_ingredients_whole() {
        // 60 "alpha" words + a period ≈ 90 tokens: +name fits, +callees would
        // overflow the 96-token cap.
        let docstring = format!("{}.", "alpha ".repeat(60).trim());
        let summary = compose_structural_summary(
            "handler",
            Some(&docstring),
            &["should_not_appear_callee".to_string()],
            r#""should not appear literal message""#,
        );
        assert!(
            summary.contains("alpha"),
            "highest-priority docstring must be retained: {summary:?}"
        );
        assert!(
            summary.contains("handler"),
            "the split name must fit alongside the docstring: {summary:?}"
        );
        assert!(
            !summary.contains("should not appear"),
            "a dropped ingredient must not appear even partially: {summary:?}"
        );
        assert!(estimate_tokens(&summary) <= SUMMARY_TOKEN_CAP);
    }

    #[test]
    fn name_only_when_no_other_ingredients() {
        let summary = compose_structural_summary("parse_config_file", None, &[], "fn f() {}");
        assert_eq!(summary, "parse config file");
    }

    #[test]
    fn byte_identical_across_repeated_calls() {
        let call = || {
            compose_structural_summary(
                "load_from_disk",
                Some("Loads state from disk."),
                &["open".to_string(), "read_to_string".to_string()],
                r#"fn f() { panic!("disk read failed"); }"#,
            )
        };
        assert_eq!(call(), call());
    }

    #[test]
    fn callee_output_order_follows_input_order_not_a_hashed_collection() {
        let a = split_callees(&["alpha".to_string(), "beta".to_string()]);
        let b = split_callees(&["beta".to_string(), "alpha".to_string()]);
        assert_eq!(a, "alpha beta");
        assert_eq!(b, "beta alpha");
    }

    #[test]
    fn split_identifier_handles_snake_camel_and_paths() {
        assert_eq!(split_identifier("retry_with_backoff"), "retry with backoff");
        assert_eq!(split_identifier("EdgeExtractor"), "edge extractor");
        assert_eq!(split_identifier("retryWithBackoff"), "retry with backoff");
        assert_eq!(
            split_identifier("EdgeExtractor::extract"),
            "edge extractor extract"
        );
    }

    #[test]
    fn first_sentence_stops_at_the_first_terminator() {
        assert_eq!(
            first_sentence("Does a thing. Then another thing."),
            "Does a thing."
        );
        assert_eq!(
            first_sentence("Multi\n  line\n  docstring with no period"),
            "Multi line docstring with no period"
        );
    }

    #[test]
    fn salient_literals_keeps_only_message_like_strings() {
        let content =
            r#"const K: &str = "x"; log("connection refused by peer"); let p = "path/to/file";"#;
        let lits = salient_literals(content);
        assert!(lits.contains("connection refused by peer"));
        // "x" is too short and single-token; "path/to/file" has no space.
        assert!(!lits.contains("path/to/file"));
        assert!(!lits.contains("\"x\""));
    }

    #[test]
    fn empty_docstring_does_not_suppress_the_name() {
        let summary = compose_structural_summary("do_thing", Some("   \n  "), &[], "fn f() {}");
        assert_eq!(summary, "do thing");
    }

    #[test]
    fn composed_summary_carries_a_secret_literal_for_the_scan_to_catch() {
        // The secret scan runs on the composed summary, not just the raw chunk.
        let summary = compose_structural_summary(
            "load_key",
            None,
            &[],
            r#"fn f() { let k = "-----BEGIN RSA PRIVATE KEY-----"; }"#,
        );
        assert!(
            summary.contains("BEGIN RSA PRIVATE KEY"),
            "the salient literal must reach the composed summary: {summary:?}"
        );
        assert!(
            crate::indexer::secrets::contains_secret(&summary),
            "the composed summary must be catchable by the secret scanner: {summary:?}"
        );
    }
}
