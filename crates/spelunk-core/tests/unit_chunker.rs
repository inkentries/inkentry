//! Unit tests for the chunker module (no I/O, no SQLite).

use spelunk_core::indexer::chunker::MAX_CHUNK_TOKENS;
use spelunk_core::indexer::{Chunk, ChunkKind, SourceParser};
use spelunk_core::search::tokens::estimate_tokens;

// ── sliding_window ───────────────────────────────────────────────────────────

#[test]
fn sliding_window_single_chunk_when_file_fits() {
    let src = "line1\nline2\nline3";
    let chunks = spelunk_core::indexer::sliding_window(src, "test.txt", "text", 10, 2);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].start_line, 1);
    assert_eq!(chunks[0].end_line, 3);
    assert_eq!(chunks[0].content, "line1\nline2\nline3");
}

#[test]
fn sliding_window_produces_overlap() {
    // 6 lines, window=4, overlap=2 → step=2
    // chunk1: lines 1-4, chunk2: lines 3-6
    let src = (1..=6)
        .map(|n| format!("line{n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = spelunk_core::indexer::sliding_window(&src, "test.txt", "text", 4, 2);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].start_line, 1);
    assert_eq!(chunks[0].end_line, 4);
    assert_eq!(chunks[1].start_line, 3);
    assert_eq!(chunks[1].end_line, 6);
}

#[test]
fn sliding_window_empty_source_returns_no_chunks() {
    let chunks = spelunk_core::indexer::sliding_window("", "test.txt", "text", 10, 2);
    assert!(chunks.is_empty());
}

#[test]
fn sliding_window_all_chunks_are_verbatim() {
    let src = "a\nb\nc\nd\ne\nf\ng\nh";
    let chunks = spelunk_core::indexer::sliding_window(src, "f.txt", "text", 3, 1);
    for c in &chunks {
        assert!(matches!(c.kind, ChunkKind::Verbatim));
    }
}

// ── Chunk::embedding_text ────────────────────────────────────────────────────

fn make_chunk(name: Option<&str>, docstring: Option<&str>, content: &str) -> Chunk {
    Chunk {
        file_path: "src/lib.rs".into(),
        language: "rust".into(),
        kind: ChunkKind::Function,
        name: name.map(str::to_string),
        start_line: 1,
        end_line: 5,
        content: content.to_string(),
        docstring: docstring.map(str::to_string),
        parent_scope: None,
        summary: None,
    }
}

#[test]
fn embedding_text_with_name() {
    let c = make_chunk(Some("my_fn"), None, "fn my_fn() {}");
    assert_eq!(c.embedding_text(), "title: my_fn | text: fn my_fn() {}");
}

#[test]
fn embedding_text_without_name_uses_none() {
    let c = make_chunk(None, None, "let x = 1;");
    assert_eq!(c.embedding_text(), "title: none | text: let x = 1;");
}

#[test]
fn embedding_text_prepends_docstring() {
    let c = make_chunk(Some("foo"), Some("/// Does foo."), "fn foo() {}");
    assert_eq!(
        c.embedding_text(),
        "title: foo | text: /// Does foo.\nfn foo() {}"
    );
}

// ── MAX_CHUNK_TOKENS ceiling ─────────────────────────────────────────────────

/// A Rust function with `body_lines` short statements, guaranteed short enough
/// per line that any 120-line window stays under the cap.
fn big_rust_fn(name: &str, body_lines: usize) -> String {
    let mut s = format!("fn {name}() {{\n");
    for i in 0..body_lines {
        s.push_str(&format!("    let v{i} = {i};\n"));
    }
    s.push_str("}\n");
    s
}

#[test]
fn oversized_leaf_splits_into_capped_subchunks() {
    // 600 short lines ≈ 2.4k tokens for the function — over the cap.
    let src = big_rust_fn("huge", 600);
    assert!(
        estimate_tokens(&src) > MAX_CHUNK_TOKENS,
        "fixture must exceed cap"
    );

    let chunks = SourceParser::parse(&src, "huge.rs", "rust").unwrap();

    // No single whole-function chunk survives; it is re-windowed.
    assert!(
        chunks.len() > 1,
        "oversized leaf should split into >1 chunk"
    );
    assert!(
        chunks.iter().all(|c| matches!(c.kind, ChunkKind::Verbatim)),
        "re-windowed sub-chunks are Verbatim"
    );
    for c in &chunks {
        assert!(
            estimate_tokens(&c.content) <= MAX_CHUNK_TOKENS,
            "sub-chunk {}-{} over cap: {} tok",
            c.start_line,
            c.end_line,
            estimate_tokens(&c.content)
        );
    }
    // Line offset preserved: the function starts at file line 1.
    assert_eq!(chunks[0].start_line, 1);
}

#[test]
fn oversized_container_suppresses_own_chunk_keeps_children() {
    // A module whose whole text is over the cap, but each child fn is under it.
    let mut src = String::from("mod tests {\n");
    for i in 0..5 {
        src.push_str(&big_rust_fn(&format!("f{i}"), 120));
    }
    src.push_str("}\n");
    assert!(
        estimate_tokens(&src) > MAX_CHUNK_TOKENS,
        "module fixture must exceed cap"
    );

    let chunks = SourceParser::parse(&src, "container.rs", "rust").unwrap();

    // Container's own Module chunk is suppressed.
    assert!(
        !chunks.iter().any(|c| matches!(c.kind, ChunkKind::Module)),
        "oversized container must not emit its own chunk"
    );
    // But per-fn child chunks are still emitted, each under the cap.
    let fns: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Function))
        .collect();
    assert_eq!(fns.len(), 5, "each child function should yield a chunk");
    for c in &fns {
        assert!(estimate_tokens(&c.content) <= MAX_CHUNK_TOKENS);
    }
    let names: Vec<&str> = fns.iter().filter_map(|c| c.name.as_deref()).collect();
    assert!(names.contains(&"f0") && names.contains(&"f4"));
}
