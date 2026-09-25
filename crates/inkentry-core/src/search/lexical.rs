//! Lexical normalisation shared by the code full-text index and its queries.
//!
//! The `chunks_fts` tokenizer (`porter unicode61`) already splits on `_` and
//! punctuation and stems, but it keeps a camelCase or PascalCase identifier
//! whole: `LinearRag` indexes as `linearrag`, which a query for `linear` or
//! `rag` never reaches. [`identifier_subwords`] supplies the missing parts at
//! write time, and [`code_fts_query`] splits the query the same way, so both
//! sides of a match agree on what a word is.

/// Words that carry no signal in a code-search query. BM25's IDF already
/// discounts them, but under OR semantics each one still admits every chunk
/// containing it as a candidate and adds a little score to it.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "code", "do", "does", "find", "for", "from",
    "function", "get", "how", "in", "into", "is", "it", "its", "of", "on", "or", "set", "that",
    "the", "this", "to", "use", "used", "using", "via", "what", "when", "where", "whether",
    "which", "why", "with",
];

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !is_word_char(c))
        .filter(|w| !w.is_empty())
}

/// Split one identifier into its lowercased parts: on `_`, at a lower-to-upper
/// boundary (`parseConfig`), before the last capital of an acronym run
/// (`HTTPServer` -> `http`, `server`), and between letters and digits.
fn split_identifier(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for segment in word.split('_') {
        let chars: Vec<char> = segment.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let (prev, cur) = (chars[i - 1], chars[i]);
            let next_is_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            let boundary = (prev.is_lowercase() && cur.is_uppercase())
                || (prev.is_uppercase() && cur.is_uppercase() && next_is_lower)
                || (prev.is_alphabetic() && cur.is_numeric())
                || (prev.is_numeric() && cur.is_alphabetic());
            if boundary {
                parts.push(chars[start..i].iter().collect::<String>().to_lowercase());
                start = i;
            }
        }
        if start < chars.len() {
            parts.push(chars[start..].iter().collect::<String>().to_lowercase());
        }
    }
    parts
}

/// The parts of every compound identifier in `text`, space-separated, for the
/// index to hold beside the text itself. Repeats are kept so BM25's term
/// frequency sees each occurrence. Words that do not split contribute nothing.
pub fn identifier_subwords(text: &str) -> String {
    let mut out = String::new();
    for word in words(text) {
        let parts = split_identifier(word);
        if parts.len() > 1 {
            for part in parts {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&part);
            }
        }
    }
    out
}

/// Build the FTS5 `MATCH` expression for a code search: each word of the query
/// lowercased, plus the parts of any compound identifier, minus stopwords,
/// deduplicated, each quoted as a literal and OR-ed so the query is scored as
/// independent terms (BM25 bag of words, never a phrase).
///
/// A query made only of stopwords keeps them rather than matching nothing.
/// Splitting on non-word characters already removes FTS5 punctuation (`:`,
/// `"`, `*`, parentheses); quoting each term is what keeps a word like `OR`,
/// `NOT` or `NEAR` a literal rather than an operator.
pub fn code_fts_query(query: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut all: Vec<String> = Vec::new();
    let push = |list: &mut Vec<String>, t: String| {
        if !list.contains(&t) {
            list.push(t);
        }
    };
    for word in words(query) {
        let lower = word.to_lowercase();
        push(&mut all, lower.clone());
        if !STOPWORDS.contains(&lower.as_str()) {
            push(&mut terms, lower);
        }
        let parts = split_identifier(word);
        if parts.len() > 1 {
            for part in parts {
                if !STOPWORDS.contains(&part.as_str()) {
                    push(&mut terms, part);
                }
            }
        }
    }
    if terms.is_empty() {
        terms = all;
    }
    if terms.is_empty() {
        return String::from("\"\"");
    }
    terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_camel_pascal_snake_acronyms_and_digits() {
        assert_eq!(split_identifier("parseConfig"), ["parse", "config"]);
        assert_eq!(split_identifier("LinearRag"), ["linear", "rag"]);
        assert_eq!(split_identifier("HTTPServer"), ["http", "server"]);
        assert_eq!(
            split_identifier("embed_query_vec"),
            ["embed", "query", "vec"]
        );
        assert_eq!(split_identifier("utf8Decode"), ["utf", "8", "decode"]);
        assert_eq!(split_identifier("plain"), ["plain"]);
    }

    #[test]
    fn subwords_hold_only_compound_identifiers() {
        assert_eq!(
            identifier_subwords("fn searchHybrid(db: Database) -> x"),
            "search hybrid"
        );
        assert_eq!(identifier_subwords("nothing to split here"), "");
    }

    #[test]
    fn subwords_keep_repeats_for_term_frequency() {
        assert_eq!(identifier_subwords("fooBar fooBar"), "foo bar foo bar");
    }

    #[test]
    fn query_drops_stopwords_and_adds_identifier_parts() {
        assert_eq!(
            code_fts_query("how does the LinearRag search work"),
            "\"linearrag\" OR \"linear\" OR \"rag\" OR \"search\" OR \"work\""
        );
    }

    #[test]
    fn query_of_only_stopwords_keeps_them() {
        assert_eq!(code_fts_query("how is it"), "\"how\" OR \"is\" OR \"it\"");
    }

    #[test]
    fn query_dedupes_terms() {
        assert_eq!(code_fts_query("bucket Bucket bucket"), "\"bucket\"");
    }

    #[test]
    fn empty_and_punctuation_only_queries_match_nothing() {
        assert_eq!(code_fts_query(""), "\"\"");
        assert_eq!(code_fts_query("  \t\n "), "\"\"");
        assert_eq!(code_fts_query("((\"\"))"), "\"\"");
    }

    #[test]
    fn fts5_syntax_in_the_query_is_split_into_plain_words() {
        assert_eq!(code_fts_query("a OR NOT b"), "\"not\" OR \"b\"");
        assert_eq!(
            code_fts_query("content:secret"),
            "\"content\" OR \"secret\""
        );
        assert_eq!(
            code_fts_query("say\"hi\0there"),
            "\"say\" OR \"hi\" OR \"there\""
        );
    }

    #[test]
    fn non_ascii_words_survive() {
        assert_eq!(
            code_fts_query("größe Übersetzung"),
            "\"größe\" OR \"übersetzung\""
        );
    }
}
