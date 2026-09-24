use super::*;

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
