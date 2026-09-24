use super::*;

#[test]
fn a_language_without_a_locals_query_still_extracts_unresolved_edges() {
    let src = r#"<html><head><script src="app.js"></script></head></html>"#;
    let edges = EdgeExtractor::extract(src, "index.html", "html").expect("extract");
    assert!(
        edges.iter().any(|e| e.target_name == "app.js"),
        "the html import edge is still extracted: {edges:?}"
    );
    assert!(edges.iter().all(|e| e.target_file.is_none()), "{edges:?}");
}

#[test]
fn a_file_over_the_parse_cap_gets_no_resolution_and_still_extracts_edges() {
    let mut src = String::from("fn helper() {}\nfn go() { helper(); }\n");
    let filler = "// padding line to push the file over the parse cap\n";
    while src.len() <= crate::indexer::parser::MAX_PARSE_BYTES {
        src.push_str(filler);
    }
    let edges = calls(&src, "src/big.rs", "rust");
    let edge = call(&edges, "go", "helper").expect("the call edge is still extracted");
    assert_eq!(edge.target_file, None);
}

#[test]
fn tsx_and_jsx_files_extract_and_resolve_call_edges() {
    let src = "\
function helper() { return 1; }
export function App() { helper(); return <div />; }
";
    for (path, language) in [("src/App.tsx", "tsx"), ("src/App.jsx", "jsx")] {
        let edges = calls(src, path, language);
        let edge = call(&edges, "App", "helper")
            .unwrap_or_else(|| panic!("{language}: App calls helper: {edges:?}"));
        assert_eq!(edge.target_file.as_deref(), Some(path));
    }
}

#[test]
fn one_caller_keeps_an_unresolved_and_a_resolved_row_for_the_same_callee() {
    // The bare call binds to the import; the receiver call reaches the
    // method this file defines. Deduplicating on the name alone would drop
    // one of the two.
    let src = "\
import { helper } from './x';
class A {
  helper() { return 1; }
  run() { helper(); this.helper(); }
}
";
    let edges = calls(src, "src/a.ts", "typescript");
    let mut targets: Vec<Option<&str>> = edges
        .iter()
        .filter(|e| e.source_name.as_deref() == Some("A") && e.target_name == "helper")
        .map(|e| e.target_file.as_deref())
        .collect();
    targets.sort();
    assert_eq!(targets, vec![None, Some("src/a.ts")], "{edges:?}");
}

#[test]
fn a_file_is_chunked_by_its_tree_unless_every_chunk_is_a_whole_file_window() {
    use crate::indexer::{SourceParser, chunker::chunked_by_tree};

    let treed = SourceParser::parse("def run():\n    return 1\n", "a.py", "python").unwrap();
    assert!(chunked_by_tree(&treed));
    let windowed = SourceParser::parse("x = 1\nprint(x)\n", "b.py", "python").unwrap();
    assert!(!chunked_by_tree(&windowed));
}

#[test]
fn an_unresolved_extraction_resolves_nothing() {
    let src = "fn helper() {}\nfn go() { helper(); }\n";
    let edges: Vec<Edge> = EdgeExtractor::extract_unresolved(src, "src/a.rs", "rust")
        .expect("extract")
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect();
    let edge = call(&edges, "go", "helper").expect("the edge is still extracted");
    assert_eq!(edge.target_file, None);
}
