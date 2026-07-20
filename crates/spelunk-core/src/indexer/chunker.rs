use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

/// The semantic kind of an extracted code chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Interface,
    Impl,
    Trait,
    Module,
    Constant,
    TypeAlias,
    /// A CSS rule set (selector + declarations block)
    Rule,
    /// A Markdown heading + its body content
    Section,
    /// Fallback: plain line range (unsupported language or oversized node)
    Verbatim,
}

impl std::fmt::Display for ChunkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "unknown".to_string());
        write!(f, "{s}")
    }
}

/// A single unit of source code to be embedded and stored.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub file_path: String,
    pub language: String,
    pub kind: ChunkKind,
    /// Symbol name, if the node has one (e.g. function or struct name).
    pub name: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    /// Optional docstring / leading comment.
    pub docstring: Option<String>,
    /// Enclosing scope (e.g. `impl MyStruct` for a method).
    pub parent_scope: Option<String>,
    /// LLM-generated one-sentence summary (set after indexing, not during parsing).
    pub summary: Option<String>,
}

impl Chunk {
    /// The text that gets passed to the embedding model.
    /// Uses EmbeddingGemma's recommended document retrieval format:
    /// `title: {title | "none"} | text: {content}`
    ///
    /// When a summary is present, it is prepended:
    /// `title: {title} | summary: {summary} | text: {content}`
    pub fn embedding_text(&self) -> String {
        let title = self.name.as_deref().unwrap_or("none");
        let body = match &self.docstring {
            Some(doc) => format!("{doc}\n{}", self.content),
            None => self.content.clone(),
        };
        match &self.summary {
            Some(summary) => format!("title: {title} | summary: {summary} | text: {body}"),
            None => format!("title: {title} | text: {body}"),
        }
    }
}

/// Soft ceiling for one chunk (tokens, chars/4 estimate). Oversized leaves are
/// re-windowed and oversized containers suppressed in favour of their children.
/// Distinct from the embedder's hard `token_cap` OOM guard.
pub const MAX_CHUNK_TOKENS: usize = 2048;

/// Process-global chunk token cap, seeded from `MAX_CHUNK_TOKENS`. All cap
/// comparisons (`chunk_token_cap()`) read this instead of the constant
/// directly, so a benchmark harness can sweep the cap without a rebuild per
/// value (see `set_chunk_token_cap`). Product code never calls the setter, so
/// shipped indexing behaviour is unchanged.
static CHUNK_TOKEN_CAP: AtomicUsize = AtomicUsize::new(MAX_CHUNK_TOKENS);

/// Current chunk token cap in effect (defaults to `MAX_CHUNK_TOKENS`).
pub fn chunk_token_cap() -> usize {
    CHUNK_TOKEN_CAP.load(Ordering::Relaxed)
}

/// Override the process-wide chunk token cap. For benchmark/eval harnesses
/// only – e.g. sweeping `MAX_CHUNK_TOKENS`-equivalent caps across a re-chunk
/// without a rebuild per value. Never call this from a production indexing
/// path, which must run on the shipped `MAX_CHUNK_TOKENS`.
pub fn set_chunk_token_cap(tokens: usize) {
    CHUNK_TOKEN_CAP.store(tokens, Ordering::Relaxed);
}

/// Split `source` into token-aware sliding-window chunks (fallback for
/// languages without a tree-sitter grammar, for files that failed parsing, and
/// for re-windowing oversized semantic nodes).
///
/// Each window accumulates whole lines while the running estimate stays within
/// [`MAX_CHUNK_TOKENS`], then starts a new window with ~12.5% token overlap
/// (matching the historical 15/120-line ratio). A single line that alone
/// exceeds the budget becomes its own window — this guarantees forward progress
/// on pathological long-line content (e.g. minified/generated code), which a
/// fixed line-count window never bounded.
///
/// `name`, `docstring`, and `parent_scope` are the identity of the source node
/// being windowed (or `None` for a whole-file fallback); they are copied onto
/// every sub-chunk so a re-windowed function still embeds with its symbol name
/// and docstring rather than `title: none`. `kind` is always
/// [`ChunkKind::Verbatim`] — a window is a partial slice, not a complete
/// instance of the parent's kind.
///
/// Enforcement is against the `chars/4` estimate ([`estimate_tokens`]), which
/// carries a corpus-dependent bias, so a window may overshoot the true token
/// count on some corpora. The goal is bounding otherwise-unbounded windows to
/// roughly the budget, not byte-exact enforcement.
///
/// [`estimate_tokens`]: crate::search::tokens::estimate_tokens
pub fn sliding_window(
    source: &str,
    file_path: &str,
    language: &str,
    name: Option<&str>,
    docstring: Option<&str>,
    parent_scope: Option<&str>,
) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    // `estimate_tokens` is `chars/4`, so the budget in characters mirrors the
    // token budget without introducing a second constant. Overlap targets
    // ~12.5% of the budget (the historical 15/120-line ratio), in tokens.
    let budget_chars: usize = chunk_token_cap() * 4;
    let overlap_chars: usize = budget_chars / 8;

    let line_chars: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();

    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < lines.len() {
        // Accumulate whole lines until the next one would exceed the budget.
        // The first line is always taken, so a single over-budget line becomes
        // its own window (forward progress on pathological long-line content).
        let mut end = start;
        let mut acc = 0usize; // characters accumulated in the current window
        while end < lines.len() {
            let add = line_chars[end] + usize::from(end > start); // +1 for the '\n' join
            if end > start && acc + add > budget_chars {
                break;
            }
            acc += add;
            end += 1;
        }

        chunks.push(Chunk {
            file_path: file_path.to_string(),
            language: language.to_string(),
            kind: ChunkKind::Verbatim,
            name: name.map(str::to_string),
            start_line: start + 1,
            end_line: end,
            content: lines[start..end].join("\n"),
            docstring: docstring.map(str::to_string),
            parent_scope: parent_scope.map(str::to_string),
            summary: None,
        });

        if end >= lines.len() {
            break;
        }

        // Start the next window ~overlap_chars earlier, on a line boundary, but
        // always strictly after the current start so the loop makes progress.
        let mut next_start = end;
        let mut overlap = 0usize;
        while next_start > start + 1 {
            let candidate = line_chars[next_start - 1] + 1; // +1 for the join '\n'
            if overlap + candidate > overlap_chars {
                break;
            }
            overlap += candidate;
            next_start -= 1;
        }
        start = next_start; // guaranteed >= start + 1 by the loop bound
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `chunk_token_cap()` reads a process-global `AtomicUsize`; tests that call
    // `set_chunk_token_cap` must run `#[serial]` (never in parallel with each
    // other) and always restore the shipped default before returning, so a
    // test failure mid-mutation can't leak a wrong cap into an unrelated test
    // running later in the same binary.
    fn with_cap<R>(tokens: usize, f: impl FnOnce() -> R) -> R {
        set_chunk_token_cap(tokens);
        let result = f();
        set_chunk_token_cap(MAX_CHUNK_TOKENS);
        result
    }

    #[test]
    #[serial]
    fn chunk_token_cap_defaults_to_max_chunk_tokens() {
        assert_eq!(chunk_token_cap(), MAX_CHUNK_TOKENS);
    }

    #[test]
    #[serial]
    fn set_chunk_token_cap_overrides_the_getter() {
        with_cap(512, || {
            assert_eq!(chunk_token_cap(), 512);
        });
        assert_eq!(
            chunk_token_cap(),
            MAX_CHUNK_TOKENS,
            "must restore the default"
        );
    }

    #[test]
    #[serial]
    fn sliding_window_respects_the_injected_cap() {
        // 200 lines of 20 chars each = ~4000 chars = ~1000 estimated tokens
        // (chars/4). Under the shipped 2048 cap this is one window; capped
        // down to 100 tokens (400 chars) it must split into several.
        let source = (0..200)
            .map(|i| format!("let line_{i:03} = 1;"))
            .collect::<Vec<_>>()
            .join("\n");

        let default_chunks = sliding_window(&source, "f.rs", "rust", None, None, None);
        assert_eq!(
            default_chunks.len(),
            1,
            "default 2048-token cap must fit this source in one window"
        );

        with_cap(100, || {
            let capped_chunks = sliding_window(&source, "f.rs", "rust", None, None, None);
            assert!(
                capped_chunks.len() > 1,
                "a 100-token cap must split the same source into multiple windows"
            );
            // Every window must individually respect the injected budget (chars/4
            // estimate, so budget_chars = 100 * 4 = 400).
            for c in &capped_chunks {
                assert!(
                    c.content.chars().count() <= 400,
                    "window content ({} chars) exceeds the 400-char budget for cap=100",
                    c.content.chars().count()
                );
            }
        });
    }
}
