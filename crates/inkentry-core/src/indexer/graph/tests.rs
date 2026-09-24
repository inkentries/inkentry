use super::{Edge, EdgeExtractor, EdgeKind};

fn calls(src: &str, path: &str, language: &str) -> Vec<Edge> {
    EdgeExtractor::extract(src, path, language)
        .expect("extract")
        .into_iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .collect()
}

fn call<'a>(edges: &'a [Edge], source: &str, target: &str) -> Option<&'a Edge> {
    edges
        .iter()
        .find(|e| e.source_name.as_deref() == Some(source) && e.target_name == target)
}

#[test]
fn a_callee_bound_in_an_enclosing_scope_resolves_to_this_file() {
    let src = "\
def outer():
    def helper():
        return 1
    return helper()
";
    let edges = calls(src, "pkg/a.py", "python");
    let edge = call(&edges, "outer", "helper").expect("outer calls helper");
    assert_eq!(edge.target_file.as_deref(), Some("pkg/a.py"));
}

#[test]
fn a_method_called_through_a_receiver_resolves_to_the_file_defining_it() {
    let src = "\
struct S;
impl S {
    fn run(&self) {}
    fn go(&self) {
        self.run();
    }
}
";
    let edges = calls(src, "src/s.rs", "rust");
    let edge = call(&edges, "go", "run").expect("go calls run");
    assert_eq!(edge.target_file.as_deref(), Some("src/s.rs"));
}

#[test]
fn a_call_to_a_name_this_file_does_not_define_stays_unresolved() {
    let src = "\
fn go() {
    helper();
}
";
    let edges = calls(src, "src/a.rs", "rust");
    let edge = call(&edges, "go", "helper").expect("go calls helper");
    assert_eq!(edge.target_file, None);
}

#[test]
fn a_callee_that_is_a_parameter_emits_no_edge_to_the_same_named_definition() {
    let src = "\
function parse(text) { return text; }
function run(parse) { return parse('x'); }
function main() { return parse('y'); }
";
    let edges = calls(src, "src/a.js", "javascript");
    assert!(
        call(&edges, "run", "parse").is_none(),
        "`parse` inside `run` is its parameter, not the function: {edges:?}"
    );
    let outside = call(&edges, "main", "parse").expect("main still calls the function");
    assert_eq!(outside.target_file.as_deref(), Some("src/a.js"));
}

#[test]
fn a_callee_that_is_a_local_variable_emits_no_edge() {
    let src = "\
def load(path):
    return path

def run():
    load = make_loader()
    return load('x')
";
    let edges = calls(src, "app/a.py", "python");
    assert!(
        call(&edges, "run", "load").is_none(),
        "`load` inside `run` is a local variable: {edges:?}"
    );
}

#[test]
fn a_typed_parameter_also_shadows() {
    let src = "\
export function parse(t: string): string { return t; }
export function run(parse: (t: string) => string): string { return parse('x'); }
";
    let edges = calls(src, "src/a.ts", "typescript");
    assert!(call(&edges, "run", "parse").is_none(), "{edges:?}");
}

#[test]
fn a_ruby_call_names_the_method_even_when_a_local_shares_its_name() {
    // `helper(1)` is a method send in Ruby whatever the local `helper` holds.
    let src = "\
class Service
  def helper(x)
    x
  end

  def run
    helper = 3
    helper(helper)
  end
end
";
    let edges = calls(src, "app/service.rb", "ruby");
    let edge = call(&edges, "run", "helper").expect("run still calls the method");
    assert_eq!(edge.target_file.as_deref(), Some("app/service.rb"));
}

#[test]
fn a_call_through_an_import_alias_targets_the_imported_name() {
    let src = "\
import { foo as bar } from './lib';
export function run() { return bar(); }
";
    let edges = calls(src, "src/a.ts", "typescript");
    assert!(call(&edges, "run", "bar").is_none(), "{edges:?}");
    let edge = call(&edges, "run", "foo").expect("run calls foo through its alias");
    assert_eq!(
        edge.target_file, None,
        "the alias names another file's definition, which this file cannot place"
    );
}

#[test]
fn a_python_import_alias_resolves_to_the_imported_name() {
    let src = "\
from lib import load as fetch

def run():
    return fetch('x')
";
    let edges = calls(src, "app/a.py", "python");
    assert!(call(&edges, "run", "load").is_some(), "{edges:?}");
}

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
fn a_destructured_require_inside_a_function_is_an_import_not_a_shadow() {
    let src = "\
function go() {
  const { helper } = require('./x');
  return helper();
}
function run() {
  const { fetch: load } = require('./y');
  return load();
}
";
    let edges = calls(src, "src/a.js", "javascript");
    let edge = call(&edges, "go", "helper").expect("go still calls the required helper");
    assert_eq!(
        edge.target_file, None,
        "the binding names another file's export"
    );
    assert!(
        call(&edges, "run", "fetch").is_some(),
        "a renaming destructure targets the required name: {edges:?}"
    );
}

#[test]
fn a_file_level_require_binding_is_not_claimed_by_the_calling_file() {
    let src = "\
const helper = require('./x');
function go() { return helper(); }
";
    let edges = calls(src, "src/a.js", "javascript");
    let edge = call(&edges, "go", "helper").expect("go calls helper");
    assert_eq!(edge.target_file, None);
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

#[test]
fn a_file_level_binding_initialised_by_a_function_resolves_to_this_file() {
    let cases = [
        (
            "javascript",
            "src/a.js",
            "const helper = () => 1;\nfunction run() { return helper(); }\n",
        ),
        (
            "javascript",
            "src/b.js",
            "const helper = function () { return 1; };\nfunction run() { return helper(); }\n",
        ),
        (
            "typescript",
            "src/c.ts",
            "const helper = class {};\nfunction run() { return helper(); }\n",
        ),
        (
            "python",
            "app/a.py",
            "helper = lambda: 1\n\ndef run():\n    return helper()\n",
        ),
        (
            "go",
            "pkg/a.go",
            "package p\n\nvar helper = func() int { return 1 }\n\nfunc run() int { return helper() }\n",
        ),
        (
            "rust",
            "src/a.rs",
            "const helper: fn() -> i32 = || 1;\nfn run() -> i32 { helper() }\n",
        ),
        (
            "cpp",
            "src/a.cpp",
            "auto helper = []() { return 1; };\nint run() { return helper(); }\n",
        ),
        (
            "csharp",
            "src/A.cs",
            "class K {\n  static Func<int> helper = () => 1;\n  static int Run() { return helper(); }\n}\n",
        ),
        (
            "kotlin",
            "src/a.kt",
            "val helper = { 1 }\nfun run() = helper()\n",
        ),
    ];
    for (language, path, src) in cases {
        let edges = calls(src, path, language);
        // C and C++ calls carry no enclosing symbol.
        let edge = edges
            .iter()
            .find(|e| e.target_name == "helper")
            .unwrap_or_else(|| panic!("{language}: the call to helper: {edges:?}"));
        assert_eq!(edge.target_file.as_deref(), Some(path), "{language}");
    }
}

#[test]
fn a_file_level_binding_initialised_by_anything_else_stays_unresolved() {
    let cases = [
        (
            "javascript",
            "src/a.js",
            "const helper = require('./x').helper;\nfunction run() { return helper(); }\n",
        ),
        (
            "javascript",
            "src/b.js",
            "const helper = require('./x').default;\nfunction run() { return helper(); }\n",
        ),
        (
            "javascript",
            "src/c.js",
            "const { helper } = await import('./x');\nfunction run() { return helper(); }\n",
        ),
        (
            "javascript",
            "src/d.js",
            "const helper = make();\nfunction run() { return helper(); }\n",
        ),
        (
            "python",
            "app/a.py",
            "helper = other.helper\n\ndef run():\n    return helper()\n",
        ),
        (
            "python",
            "app/b.py",
            "helper = 3\n\ndef run():\n    return helper()\n",
        ),
        (
            "go",
            "pkg/a.go",
            "package p\n\nvar helper = other.Helper\n\nfunc run() int { return helper() }\n",
        ),
    ];
    for (language, path, src) in cases {
        let edges = calls(src, path, language);
        let edge = call(&edges, "run", "helper")
            .unwrap_or_else(|| panic!("{path}: the edge is kept: {edges:?}"));
        assert_eq!(edge.target_file, None, "{path}");
    }
}

#[test]
fn a_python_class_attribute_does_not_bind_a_bare_call_inside_a_method() {
    // A method body sees module and builtin names, never the class body's.
    let with_module_def = "\
def helper():
    return 1

class A:
    helper = staticmethod(helper)

    def go(self):
        return helper()
";
    let edges = calls(with_module_def, "app/a.py", "python");
    let edge = call(&edges, "go", "helper").expect("go calls the module-level helper");
    assert_eq!(edge.target_file.as_deref(), Some("app/a.py"));

    let free = "\
class A:
    helper = staticmethod(make)

    def go(self):
        return helper()
";
    let edges = calls(free, "app/b.py", "python");
    let edge = call(&edges, "go", "helper").expect("the free call keeps its edge");
    assert_eq!(edge.target_file, None);
}

#[test]
fn a_bare_call_never_reaches_a_method_where_the_language_needs_a_receiver() {
    let cases = [
        (
            "python",
            "app/a.py",
            "class A:\n    def helper(self):\n        return 1\n\n    def go(self):\n        return helper()\n",
        ),
        (
            "javascript",
            "src/a.js",
            "class A {\n  helper() { return 1; }\n  go() { return helper(); }\n}\n",
        ),
        (
            "javascript",
            "src/b.js",
            "const o = { helper() { return 1; } };\nfunction go() { return helper(); }\n",
        ),
        (
            "rust",
            "src/a.rs",
            "struct S;\nimpl S {\n    fn helper(&self) {}\n    fn go(&self) { helper(); }\n}\n",
        ),
        (
            "go",
            "pkg/a.go",
            "package p\n\ntype S struct{}\n\nfunc (s S) helper() {}\n\nfunc run() { helper() }\n",
        ),
    ];
    for (language, path, src) in cases {
        let edges = calls(src, path, language);
        let edge = edges
            .iter()
            .find(|e| e.target_name == "helper")
            .unwrap_or_else(|| panic!("{path}: the call keeps its edge: {edges:?}"));
        assert_eq!(edge.target_file, None, "{path}");
    }
}

#[test]
fn a_nested_function_is_not_reached_from_outside_its_enclosing_function() {
    let src = "\
function outer() {
  function inner() { return 1; }
  return inner();
}
function other() { return inner(); }
";
    let edges = calls(src, "src/a.js", "javascript");
    let within = call(&edges, "outer", "inner").expect("outer calls inner");
    assert_eq!(within.target_file.as_deref(), Some("src/a.js"));
    let outside = call(&edges, "other", "inner").expect("other's call keeps its edge");
    assert_eq!(outside.target_file, None);
}

#[test]
fn a_bare_call_reaches_a_method_of_the_same_class_where_the_receiver_is_implicit() {
    let src = "\
class A {
  int helper() { return 1; }
  int go() { return helper(); }
}
class B {
  int run() { return helper(); }
}
";
    let edges = calls(src, "src/A.java", "java");
    let same = call(&edges, "go", "helper").expect("go calls helper");
    assert_eq!(same.target_file.as_deref(), Some("src/A.java"));
    let other = call(&edges, "run", "helper").expect("B's call keeps its edge");
    assert_eq!(other.target_file, None, "B's body does not see A's methods");
}
