use super::*;

#[test]
fn a_method_called_through_self_resolves_to_the_file_defining_it() {
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
fn a_non_self_receiver_call_to_a_method_this_file_defines_stays_unresolved() {
    // Which `find` runs depends on the receiver's type, which the file does
    // not decide.
    let cases = [
        (
            "python",
            "app/a.py",
            "class A:\n    def find(self):\n        return 1\n\n    def go(self, items):\n        return items.find()\n",
        ),
        (
            "javascript",
            "src/a.js",
            "class A {\n  find() { return 1; }\n  go(o) { return o.find(); }\n}\n",
        ),
        (
            "rust",
            "src/a.rs",
            "struct S;\nimpl S {\n    fn find(&self) {}\n    fn go(&self, o: &S) { o.find(); }\n}\n",
        ),
        (
            "java",
            "src/A.java",
            "class A {\n  int find() { return 1; }\n  int go(A o) { return o.find(); }\n}\n",
        ),
    ];
    for (language, path, src) in cases {
        let edges = calls(src, path, language);
        assert_eq!(edge_to(&edges, "find", path).target_file, None, "{path}");
    }
}

#[test]
fn a_self_receiver_call_resolves_to_the_method_of_the_same_type() {
    let cases = [
        (
            "python",
            "app/a.py",
            "class A:\n    def m(self):\n        return 1\n\n    def go(self):\n        return self.m()\n",
        ),
        (
            "python",
            "app/b.py",
            "class A:\n    @classmethod\n    def m(cls):\n        return 1\n\n    @classmethod\n    def go(cls):\n        return cls.m()\n",
        ),
        (
            "typescript",
            "src/a.ts",
            "class A {\n  m() { return 1; }\n  go() { return this.m(); }\n}\n",
        ),
        (
            "java",
            "src/A.java",
            "class A {\n  int m() { return 1; }\n  int go() { return this.m(); }\n}\n",
        ),
        (
            "csharp",
            "src/A.cs",
            "class A {\n  int M() { return 1; }\n  int Go() { return this.M(); }\n}\n",
        ),
        (
            "php",
            "src/A.php",
            "<?php\nclass A {\n  function m() { return 1; }\n  function go() { return $this->m(); }\n}\n",
        ),
        (
            "ruby",
            "app/a.rb",
            "class A\n  def m\n    1\n  end\n\n  def go\n    self.m\n  end\nend\n",
        ),
        (
            "rust",
            "src/a.rs",
            "struct S;\nimpl S {\n    fn m() {}\n    fn go(&self) { Self::m(); }\n}\n",
        ),
        (
            "swift",
            "src/A.swift",
            "class A {\n  func m() -> Int { return 1 }\n  func go() -> Int { return self.m() }\n}\n",
        ),
    ];
    for (language, path, src) in cases {
        let edges = calls(src, path, language);
        let target = if language == "csharp" { "M" } else { "m" };
        assert_eq!(
            edge_to(&edges, target, path).target_file.as_deref(),
            Some(path),
            "{path}"
        );
    }
}

#[test]
fn a_self_receiver_call_to_a_method_only_another_type_defines_stays_unresolved() {
    let src = "\
class B:
    def m(self):
        return 1

class A:
    def go(self):
        return self.m()
";
    let edges = calls(src, "app/a.py", "python");
    assert_eq!(edge_to(&edges, "m", "app/a.py").target_file, None);
}

#[test]
fn a_go_method_calling_through_its_own_receiver_resolves_to_this_file() {
    let src = "\
package p

type S struct{}

func (s S) m() int { return 1 }

func (s *S) run() int { return s.m() }

func other(t S) int { return t.m() }
";
    let edges = calls(src, "pkg/s.go", "go");
    let own = edges
        .iter()
        .find(|e| e.source_name.as_deref() == Some("run"))
        .unwrap_or_else(|| panic!("run calls s.m: {edges:?}"));
    assert_eq!(own.target_file.as_deref(), Some("pkg/s.go"));
    let foreign = edges
        .iter()
        .find(|e| e.source_name.as_deref() == Some("other"))
        .unwrap_or_else(|| panic!("other calls t.m: {edges:?}"));
    assert_eq!(
        foreign.target_file, None,
        "`t` is a parameter, not the receiver"
    );
}

fn target_of(src: &str, path: &str, language: &str, source: &str, target: &str) -> Option<String> {
    let edges = calls(src, path, language);
    edges
        .iter()
        .find(|e| e.source_name.as_deref() == Some(source) && e.target_name == target)
        .unwrap_or_else(|| panic!("{path}: {source} calls {target}: {edges:?}"))
        .target_file
        .clone()
}

#[test]
fn a_self_reference_rebound_between_the_call_and_the_method_stays_unresolved() {
    let cases = [
        // A `function` rebinds `this`.
        (
            "javascript",
            "src/a.js",
            "class A {\n  m() {}\n  go() { items.forEach(function () { this.m(); }); }\n}\n",
            "A",
        ),
        (
            "javascript",
            "src/b.js",
            "class A {\n  m() {}\n  go() { el.onclick = function () { this.m(); }; }\n}\n",
            "A",
        ),
        // A nested def or lambda with its own `self` is not the method.
        (
            "python",
            "app/a.py",
            "class A:\n    def m(self):\n        return 1\n\n    def go(self):\n        def inner(self):\n            return self.m()\n        return inner\n",
            "inner",
        ),
        (
            "python",
            "app/b.py",
            "class A:\n    def m(self):\n        return 1\n\n    def go(self):\n        f = lambda self: self.m()\n        return f\n",
            "go",
        ),
        (
            "python",
            "app/c.py",
            "class A:\n    def m(self):\n        return 1\n\n    def go(self, other):\n        self = other\n        return self.m()\n",
            "go",
        ),
        // `this` in an anonymous class body is the anonymous type.
        (
            "java",
            "src/A.java",
            "class A {\n  int m() { return 1; }\n  Object go() { return new Runnable() { public void run() { this.m(); } }; }\n}\n",
            "run",
        ),
    ];
    for (language, path, src, source) in cases {
        assert_eq!(target_of(src, path, language, source, "m"), None, "{path}");
    }
}

#[test]
fn an_arrow_function_keeps_the_methods_this() {
    let src = "class A {\n  m() {}\n  go() { items.forEach(() => this.m()); }\n}\n";
    assert_eq!(
        target_of(src, "src/a.js", "javascript", "A", "m").as_deref(),
        Some("src/a.js")
    );
}

#[test]
fn a_go_receiver_shadowed_before_the_call_stays_unresolved() {
    let src = "\
package p

type S struct{}

func (s S) m() int { return 1 }

func (s *S) viaParam(t S) int { return t.m() }

func (s *S) viaClosure() func(S) int { return func(s S) int { return s.m() } }

func (s *S) viaShortVar() int { s := S{}; return s.m() }

func (s *S) viaRange(xs []S) int { for _, s := range xs { return s.m() }; return 0 }
";
    let edges = calls(src, "pkg/s.go", "go");
    for source in ["viaParam", "viaClosure", "viaShortVar", "viaRange"] {
        let edge = edges
            .iter()
            .find(|e| e.source_name.as_deref() == Some(source))
            .unwrap_or_else(|| panic!("{source} calls m: {edges:?}"));
        assert_eq!(edge.target_file, None, "{source}");
    }
}

#[test]
fn a_call_qualified_by_a_type_this_file_defines_resolves_to_it() {
    let cases = [
        (
            "rust",
            "src/a.rs",
            "struct Foo;\nimpl Foo {\n    fn new() -> Foo { Foo }\n}\nfn go() { Foo::new(); }\n",
            "go",
            "new",
        ),
        (
            "rust",
            "src/b.rs",
            "fn helper() {}\nmod inner {}\nfn go() { self::helper(); }\n",
            "go",
            "helper",
        ),
        (
            "php",
            "src/A.php",
            "<?php\nclass A {\n  static function m() { return 1; }\n  function go() { return self::m(); }\n}\n",
            "go",
            "m",
        ),
        (
            "python",
            "app/a.py",
            "class A:\n    @staticmethod\n    def m():\n        return 1\n\ndef go():\n    return A.m()\n",
            "go",
            "m",
        ),
        (
            "typescript",
            "src/a.ts",
            "class A {\n  static m() { return 1; }\n}\nexport function go() { return A.m(); }\n",
            "go",
            "m",
        ),
        (
            "java",
            "src/B.java",
            "class A {\n  static int m() { return 1; }\n}\nclass B {\n  int go() { return A.m(); }\n}\n",
            "go",
            "m",
        ),
    ];
    for (language, path, src, source, target) in cases {
        assert_eq!(
            target_of(src, path, language, source, target).as_deref(),
            Some(path),
            "{path}"
        );
    }
    let foo = "struct Foo;\nimpl Foo {\n    fn new() -> Foo { Foo }\n}\nfn go() { Foo::new(); }\n";
    assert_eq!(
        target_of(foo, "src/a.rs", "rust", "go", "Foo").as_deref(),
        Some("src/a.rs"),
        "the path's own edge names the type the file defines"
    );
}

#[test]
fn a_qualifier_that_is_not_a_statically_named_type_here_stays_unresolved() {
    let cases = [
        // Late static binding can dispatch to a subclass elsewhere.
        (
            "php",
            "src/A.php",
            "<?php\nclass A {\n  static function m() { return 1; }\n  function go() { return static::m(); }\n}\n",
            "go",
        ),
        // `A` is the parameter here, not the class.
        (
            "javascript",
            "src/a.js",
            "class A {\n  static m() { return 1; }\n}\nfunction go(A) { return A.m(); }\n",
            "go",
        ),
        // A type this file does not define.
        (
            "java",
            "src/B.java",
            "class B {\n  int go() { return Other.m(); }\n  int m() { return 1; }\n}\n",
            "go",
        ),
    ];
    for (language, path, src, source) in cases {
        assert_eq!(target_of(src, path, language, source, "m"), None, "{path}");
    }
}

#[test]
fn methods_of_two_anonymous_types_are_told_apart() {
    let src = "\
const A = class {
  m() {}
  go() { return this.m(); }
};
const B = class {
  go() { return this.m(); }
};
";
    let edges = calls(src, "src/a.js", "javascript");
    let mut targets: Vec<Option<&str>> = edges
        .iter()
        .filter(|e| e.target_name == "m")
        .map(|e| e.target_file.as_deref())
        .collect();
    targets.sort();
    assert_eq!(
        targets,
        vec![None, Some("src/a.js")],
        "only A defines m: {edges:?}"
    );
}
