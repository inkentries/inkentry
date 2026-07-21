use std::collections::HashSet;
use std::sync::OnceLock;

static MENTION_STOPWORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();

fn mention_stopwords() -> &'static HashSet<&'static str> {
    MENTION_STOPWORDS.get_or_init(|| {
        [
            // Rust
            "fn",
            "let",
            "mut",
            "pub",
            "use",
            "mod",
            "struct",
            "enum",
            "impl",
            "trait",
            "type",
            "where",
            "async",
            "await",
            "move",
            "dyn",
            "ref",
            "box",
            "unsafe",
            "true",
            "false",
            "self",
            "super",
            "crate",
            "extern",
            // Common types / builtins
            "None",
            "Some",
            "Ok",
            "Err",
            "Vec",
            "String",
            "str",
            "bool",
            "usize",
            "i32",
            "i64",
            "u32",
            "u64",
            "f32",
            "f64",
            "isize",
            "u8",
            "i8",
            "u16",
            "i16",
            // Python
            "def",
            "class",
            "import",
            "from",
            "with",
            "pass",
            "raise",
            "yield",
            "lambda",
            "global",
            "nonlocal",
            "assert",
            "del",
            // JavaScript / TypeScript
            "var",
            "const",
            "function",
            "typeof",
            "instanceof",
            "new",
            "delete",
            "export",
            "default",
            "extends",
            "static",
            // Go
            "func",
            "package",
            "interface",
            "chan",
            "select",
            "defer",
            "goto",
            "fallthrough",
            // Java / C
            "void",
            "null",
            "class",
            "int",
            "long",
            "double",
            "float",
            "char",
            "byte",
            "short",
            "final",
            "this",
            "super",
            "throws",
            "throw",
            "catch",
            "finally",
            // Control flow (shared)
            "if",
            "else",
            "for",
            "while",
            "do",
            "switch",
            "case",
            "break",
            "continue",
            "return",
            "match",
            "in",
            "not",
            "and",
            "or",
            "is",
            // Very common but not meaningful
            "get",
            "set",
            "add",
            "new",
            "into",
            "from",
            "with",
            "data",
            "val",
        ]
        .iter()
        .copied()
        .collect()
    })
}

struct CommentStyle {
    line_prefixes: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
}

/// Comment syntax per language, for `strip_comments_and_strings`. `None` means the
/// language is unrecognized here and content passes through unstripped (fail open).
fn comment_style(language: &str) -> Option<CommentStyle> {
    match language {
        "rust" | "javascript" | "jsx" | "typescript" | "tsx" | "go" | "java" | "c" | "cpp"
        | "csharp" | "kotlin" | "swift" | "proto" => Some(CommentStyle {
            line_prefixes: &["//"],
            block: Some(("/*", "*/")),
        }),
        "python" | "ruby" => Some(CommentStyle {
            line_prefixes: &["#"],
            block: None,
        }),
        "php" | "hcl" => Some(CommentStyle {
            line_prefixes: &["//", "#"],
            block: Some(("/*", "*/")),
        }),
        "sql" => Some(CommentStyle {
            line_prefixes: &["--"],
            block: Some(("/*", "*/")),
        }),
        "css" => Some(CommentStyle {
            line_prefixes: &[],
            block: Some(("/*", "*/")),
        }),
        _ => None,
    }
}

/// Does `chars[i..]` start with `pat`?
fn matches_at(chars: &[char], i: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(offset, pc)| chars.get(i + offset) == Some(&pc))
}

/// Strip comment and string/char-literal spans for `language` before mention
/// tokenization, so comment prose and string contents never reach the tokenizer.
/// Stripped spans become a single space, preserving token boundaries on either
/// side. Unrecognized languages pass through unchanged.
fn strip_comments_and_strings(content: &str, language: &str) -> String {
    let Some(style) = comment_style(language) else {
        return content.to_string();
    };

    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;

    while i < n {
        if let Some((open, close)) = style.block
            && matches_at(&chars, i, open)
        {
            i += open.chars().count();
            while i < n && !matches_at(&chars, i, close) {
                i += 1;
            }
            i = (i + close.chars().count()).min(n);
            out.push(' ');
            continue;
        }

        let mut line_prefix: Option<&str> = None;
        for prefix in style.line_prefixes {
            if matches_at(&chars, i, prefix) {
                line_prefix = Some(prefix);
                break;
            }
        }
        if let Some(prefix) = line_prefix {
            i += prefix.chars().count();
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        let ch = chars[i];
        if ch == '"' || ch == '\'' || ch == '`' {
            i += 1;
            while i < n {
                if chars[i] == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if chars[i] == ch {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(' ');
            continue;
        }

        out.push(ch);
        i += 1;
    }

    out
}

/// Extract identifier-like tokens from chunk content for use as mention edges.
/// Returns up to 40 unique tokens that look like symbol names.
pub(super) fn extract_mention_tokens(content: &str, language: &str) -> Vec<String> {
    let stripped = strip_comments_and_strings(content, language);
    let stop = mention_stopwords();
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let mut start: Option<usize> = None;
    let chars: Vec<char> = stripped.chars().collect();
    let n = chars.len();

    for i in 0..=n {
        let ch = if i < n { chars[i] } else { ' ' };
        let is_ident = ch.is_ascii_alphanumeric() || ch == '_';

        if is_ident {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            let tok: String = chars[s..i].iter().collect();
            // Keep tokens that look like symbols: 3-50 chars, not all digits, not a stopword
            if tok.len() >= 3
                && tok.len() <= 50
                && !tok.chars().all(|c| c.is_ascii_digit())
                && !stop.contains(tok.as_str())
                && seen.insert(tok.clone())
            {
                out.push(tok);
                if out.len() >= 40 {
                    break;
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_rust_line_comment_keeps_preceding_code() {
        let out = strip_comments_and_strings("let dirPath = x; // Use forward slashes", "rust");
        assert!(out.contains("dirPath"));
        assert!(!out.contains("Use"));
        assert!(!out.contains("forward"));
        assert!(!out.contains("slashes"));
    }

    #[test]
    fn strips_block_comment_single_and_multiline() {
        let single = strip_comments_and_strings("foo /* inline note */ bar", "rust");
        assert!(single.contains("foo"));
        assert!(single.contains("bar"));
        assert!(!single.contains("inline"));

        let multi = strip_comments_and_strings("foo /* spans\nmultiple\nlines */ bar", "rust");
        assert!(multi.contains("foo"));
        assert!(multi.contains("bar"));
        assert!(!multi.contains("spans"));
        assert!(!multi.contains("multiple"));
    }

    #[test]
    fn strips_python_hash_line_comment() {
        let out = strip_comments_and_strings("dir_path = x  # Ignore hidden dotfiles", "python");
        assert!(out.contains("dir_path"));
        assert!(!out.contains("Ignore"));
        assert!(!out.contains("hidden"));
        assert!(!out.contains("dotfiles"));
    }

    #[test]
    fn strips_string_literal_keeps_identifier() {
        let out = strip_comments_and_strings(
            r#"const IMAGE_EXTS = ["png", "jpg", "jpeg", "gif", "webp"];"#,
            "javascript",
        );
        assert!(out.contains("IMAGE_EXTS"));
        for ext in ["png", "jpg", "jpeg", "gif", "webp"] {
            assert!(!out.contains(ext));
        }
    }

    #[test]
    fn escaped_quote_does_not_end_string_early() {
        let out = strip_comments_and_strings(r#"let s = "a \"quoted\" word"; keep_me"#, "rust");
        assert!(out.contains("keep_me"));
        assert!(!out.contains("quoted"));
        assert!(!out.contains("word"));
    }

    #[test]
    fn unrecognized_language_passes_through_unchanged() {
        let content = "// not actually stripped\nlet x = \"stays\";";
        assert_eq!(strip_comments_and_strings(content, "cobol"), content);
        assert_eq!(strip_comments_and_strings(content, ""), content);
    }

    #[test]
    fn extract_mention_tokens_excludes_comment_prose() {
        let content = "\
// Use forward slashes for paths, not backslashes.
// Ignore hidden dotfiles when reading the directory tree.
fn getDirectoryTree(dirPath: string) { const subTree = walk(dirPath); return subTree; }
";
        let tokens = extract_mention_tokens(content, "typescript");
        for junk in [
            "the", "Use", "Ignore", "reading", "forward", "slashes", "hidden",
        ] {
            assert!(
                !tokens.iter().any(|t| t == junk),
                "unexpected junk token: {junk}"
            );
        }
        for real in ["dirPath", "subTree", "getDirectoryTree", "walk"] {
            assert!(
                tokens.iter().any(|t| t == real),
                "missing expected token: {real}"
            );
        }
    }

    #[test]
    fn extract_mention_tokens_excludes_extension_string_literal() {
        let content = r#"const IMAGE_EXTS = ["png", "jpg", "jpeg", "gif", "webp"];"#;
        let tokens = extract_mention_tokens(content, "javascript");
        assert!(tokens.iter().any(|t| t == "IMAGE_EXTS"));
        for ext in ["png", "jpg", "jpeg", "gif", "webp"] {
            assert!(!tokens.iter().any(|t| t == ext));
        }
    }

    #[test]
    fn extract_mention_tokens_still_filters_existing_stopwords() {
        let content = "fn helper() { let value = 1; pub fn other(self) {} }";
        let tokens = extract_mention_tokens(content, "rust");
        for stopword in ["fn", "let", "pub", "self"] {
            assert!(!tokens.iter().any(|t| t == stopword));
        }
    }
}
