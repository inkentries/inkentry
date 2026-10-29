use super::*;

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

#[test]
fn an_out_of_line_member_is_not_reached_by_a_bare_call_outside_its_type() {
    let src = "\
class A { void run(); };
void A::run() {}
void other() { run(); }
";
    let edges = calls(src, "src/a.cpp", "cpp");
    let edge = edges
        .iter()
        .find(|e| e.target_name == "run")
        .unwrap_or_else(|| panic!("other calls run: {edges:?}"));
    assert_eq!(edge.target_file, None);
}
