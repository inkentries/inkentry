mod text;
mod ts_walker;

use super::chunker::{Chunk, sliding_window};
use anyhow::Result;
use std::ops::ControlFlow;

/// All languages recognised by the indexer (tree-sitter, text, and document formats).
pub const SUPPORTED_LANGUAGES: &[&str] = &[
    "rust",
    "python",
    "javascript",
    "jsx",
    "typescript",
    "tsx",
    "go",
    "java",
    "c",
    "cpp",
    "json",
    "html",
    "css",
    "hcl",
    "php",
    "ruby",
    "csharp",
    "kotlin",
    "swift",
    "sql",
    "proto",
    // text formats (sliding-window / heading-based, no tree-sitter)
    "markdown",
    "text",
    // structured text (custom parsers, no tree-sitter)
    "notebook",
    // binary document formats (docparser, no tree-sitter)
    #[cfg(feature = "rich-formats")]
    "docx",
    #[cfg(feature = "rich-formats")]
    "spreadsheet",
    // PDF (rich-formats feature)
    #[cfg(feature = "rich-formats")]
    "pdf",
];

/// Detect language from file extension.
pub fn detect_language(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "jsx" => Some("jsx"),
        "ts" | "mts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some("cpp"),
        "json" => Some("json"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "tf" | "hcl" => Some("hcl"),
        "php" | "phtml" => Some("php"),
        "rb" | "rake" | "gemspec" => Some("ruby"),
        "cs" => Some("csharp"),
        "kt" | "kts" => Some("kotlin"),
        "swift" => Some("swift"),
        "sql" | "sequel" => Some("sql"),
        "proto" => Some("proto"),
        #[cfg(feature = "rich-formats")]
        "pdf" => Some("pdf"),
        _ => None,
    }
}

/// Inputs larger than this are chunked by sliding window instead of parsed.
/// Guards against adversarial inputs that make tree-sitter's GLR parser
/// allocate exponential memory (e.g. deeply-nested pointer declarators): the
/// parse's time budget only bounds CPU time, and memory can spike before its
/// first progress callback fires.
pub(crate) const MAX_PARSE_BYTES: usize = 512 * 1024;

pub(crate) fn ts_language_pub(name: &str) -> Result<tree_sitter::Language> {
    ts_walker::ts_language(name)
}

/// Detect text-format languages (markdown, plain text, notebooks) from file path.
/// These are handled without tree-sitter.
pub fn detect_text_language(path: &std::path::Path) -> Option<&'static str> {
    // Check extension first
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        return match ext.to_lowercase().as_str() {
            "md" | "mdx" | "markdown" => Some("markdown"),
            // R Markdown / Quarto documents are markdown with fenced code blocks.
            "rmd" | "qmd" => Some("markdown"),
            "txt" | "rst" | "adoc" | "asciidoc" => Some("text"),
            // Jupyter notebooks: custom JSON-based parser.
            "ipynb" => Some("notebook"),
            _ => None,
        };
    }
    // Extensionless files: README, CHANGELOG, etc.
    let name = path.file_name()?.to_str()?.to_uppercase();
    match name.as_str() {
        "README" | "CHANGELOG" | "CHANGES" | "CONTRIBUTING" | "NOTICE" | "AUTHORS" | "HISTORY" => {
            Some("text")
        }
        _ => None,
    }
}

/// Detect binary document formats (DOCX, spreadsheets) from file extension.
/// These are handled by `docparser` — they cannot be read with `read_to_string`.
/// Only returns `Some` when the `rich-formats` feature is enabled.
pub fn detect_doc_language(path: &std::path::Path) -> Option<&'static str> {
    #[cfg(feature = "rich-formats")]
    match path.extension()?.to_str()?.to_lowercase().as_str() {
        "docx" => return Some("docx"),
        "xlsx" | "xls" | "ods" => return Some("spreadsheet"),
        _ => {}
    }
    let _ = path;
    None
}

/// Return true if the file appears to be binary (contains null bytes in the
/// first 512 bytes). Used to skip compiled or binary assets.
pub fn is_binary_file(path: &std::path::Path) -> bool {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut buf = [0u8; 512];
        if let Ok(n) = f.read(&mut buf) {
            return buf[..n].contains(&0u8);
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct SourceParser;

impl SourceParser {
    /// Parse `source` and return semantic chunks.
    /// Falls back to sliding-window if parsing fails or yields nothing.
    pub fn parse(source: &str, file_path: &str, language: &str) -> Result<Vec<Chunk>> {
        // Text formats bypass tree-sitter entirely.
        if language == "markdown" {
            return Ok(text::parse_markdown(source, file_path));
        }
        if language == "text" {
            return Ok(sliding_window(
                source, file_path, language, None, None, None,
            ));
        }
        if language == "notebook" {
            return Ok(text::parse_notebook(source, file_path));
        }

        if source.len() > MAX_PARSE_BYTES {
            tracing::warn!(
                "{file_path}: input too large ({} bytes > {MAX_PARSE_BYTES}), using sliding window",
                source.len()
            );
            return Ok(sliding_window(
                source, file_path, language, None, None, None,
            ));
        }

        let ts_lang = ts_walker::ts_language(language)?;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang)?;

        // Prevent pathological inputs (adversarial or deeply ambiguous) from
        // consuming unbounded memory/time during GLR parsing.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut on_progress = |_: &tree_sitter::ParseState| -> ControlFlow<()> {
            if std::time::Instant::now() >= deadline {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let bytes = source.as_bytes();
        let len = bytes.len();
        let mut opts = tree_sitter::ParseOptions::new().progress_callback(&mut on_progress);
        let tree = match parser.parse_with_options(
            &mut |i, _| if i < len { &bytes[i..] } else { &[] },
            None,
            Some(opts.reborrow()),
        ) {
            Some(t) => t,
            None => {
                tracing::warn!(
                    "{file_path}: tree-sitter parse exceeded time budget, using sliding window"
                );
                return Ok(sliding_window(
                    source, file_path, language, None, None, None,
                ));
            }
        };

        let specs = ts_walker::node_specs(language);
        let ctx = ts_walker::WalkCtx {
            src: bytes,
            file_path,
            language,
            specs: &specs,
        };
        let mut walked = ts_walker::Walked::default();

        ts_walker::walk_node(tree.root_node(), &ctx, None, &mut walked, 0);

        if walked.chunks.is_empty() {
            tracing::debug!("{file_path}: no semantic nodes found, using sliding window");
            return Ok(sliding_window(
                source, file_path, language, None, None, None,
            ));
        }

        let mut chunks = walked.chunks;
        fill_gaps(source, file_path, language, &walked.scopes, &mut chunks);
        for chunk in &mut chunks {
            chunk.in_test_code = walked
                .test_spans
                .iter()
                .any(|&(start, end)| start <= chunk.start_line && chunk.end_line <= end);
        }
        Ok(chunks)
    }
}

/// A gap with fewer letters and digits than this is left out: a closing `}`
/// or `end`, a lone `private`. Measured over the whole gap (within one
/// container), so a run of short lines (`has_many :fees`, one per line) still
/// qualifies.
const MIN_GAP_WORD_CHARS: usize = 16;

/// Window every stretch of `source` no chunk covers, so code outside a matched
/// node is still indexed: module-level statements and constants, and the body
/// of a container too large to keep whole (whose own chunk is suppressed in
/// favour of its members), such as a Rails model's associations and
/// validations. A chunk's docstring counts as covering the lines above it.
///
/// A stretch is cut where it crosses the boundary of a suppressed container,
/// so every window lies in one container or none; the one exception is a
/// container's bare header, which stays with the nested container it opens.
/// The window holding a container's declaration is named after it, with its
/// own `parent_scope`, as the container's re-windowed chunk would be. Windows
/// between its members stay unnamed: there can be dozens (`private`,
/// `delegate`, `attr_reader`), and one name on all of them crowds the
/// container's members out of any query naming it.
fn fill_gaps(
    source: &str,
    file_path: &str,
    language: &str,
    scopes: &[ts_walker::SuppressedScope],
    chunks: &mut Vec<Chunk>,
) {
    let lines: Vec<&str> = source.lines().collect();
    let mut owner: Vec<Option<usize>> = vec![None; lines.len() + 1];
    // Walk order puts an outer container first, so an inner one overwrites it.
    for (i, scope) in scopes.iter().enumerate() {
        let to = scope.end_line.min(lines.len());
        if scope.start_line <= to {
            owner[scope.start_line..=to].fill(Some(i));
        }
    }
    let mut covered = vec![false; lines.len() + 1];
    for chunk in chunks.iter() {
        let doc_lines = chunk.docstring.as_deref().map_or(0, |d| d.lines().count());
        let from = chunk.start_line.saturating_sub(doc_lines).max(1);
        let to = chunk.end_line.min(lines.len());
        if from <= to {
            covered[from..=to].fill(true);
        }
    }

    let mut gaps: Vec<Chunk> = Vec::new();
    let mut line = 1;
    while line <= lines.len() {
        if covered[line] || lines[line - 1].trim().is_empty() {
            line += 1;
            continue;
        }
        let start = line;
        let mut end = line;
        let mut here = owner[start];
        while line <= lines.len() && !covered[line] {
            if owner[line] != here {
                // `module Billing` / `module Invoices` / `class CreateService`
                // stays one window: a bare header is not a body of its own.
                let opens_nested = match (here, owner[line]) {
                    (Some(outer), Some(inner)) => {
                        end <= scopes[outer].decl_line && scopes[outer].contains(&scopes[inner])
                    }
                    _ => false,
                };
                if !opens_nested {
                    break;
                }
                here = owner[line];
            }
            if !lines[line - 1].trim().is_empty() {
                end = line;
            }
            line += 1;
        }
        let text = lines[start - 1..end].join("\n");
        if text.chars().filter(|c| c.is_alphanumeric()).count() < MIN_GAP_WORD_CHARS {
            continue;
        }
        let scope = here
            .map(|i| &scopes[i])
            .filter(|s| (start..=end).contains(&s.decl_line));
        for mut window in sliding_window(
            &text,
            file_path,
            language,
            scope.map(|s| s.name.as_str()),
            None,
            scope.and_then(|s| s.parent_scope.as_deref()),
        ) {
            window.start_line += start - 1;
            window.end_line += start - 1;
            gaps.push(window);
        }
    }
    if !gaps.is_empty() {
        chunks.extend(gaps);
        chunks.sort_by_key(|c| c.start_line);
    }
}

#[cfg(test)]
mod tests {
    use super::SUPPORTED_LANGUAGES;

    // Pinning this constant is an indirection, not a guarantee: it cannot tell
    // whether the documented lists are right, only that nobody can change the
    // set without being reminded they exist. `rich-formats` is checked with
    // `cfg!` so both arms compile in either build and neither can rot unseen.
    #[test]
    fn supported_languages_pinned_against_documented_lists() {
        let mut expected = vec![
            "rust",
            "python",
            "javascript",
            "jsx",
            "typescript",
            "tsx",
            "go",
            "java",
            "c",
            "cpp",
            "json",
            "html",
            "css",
            "hcl",
            "php",
            "ruby",
            "csharp",
            "kotlin",
            "swift",
            "sql",
            "proto",
            "markdown",
            "text",
            "notebook",
        ];
        if cfg!(feature = "rich-formats") {
            expected.extend(["docx", "spreadsheet", "pdf"]);
        }
        assert_eq!(
            SUPPORTED_LANGUAGES,
            expected.as_slice(),
            "SUPPORTED_LANGUAGES changed. Update the language lists in README.md \
             (section 'Supported languages') and CLAUDE.md (section 'Supported \
             Languages'), then update this expected array to match."
        );
    }
}
