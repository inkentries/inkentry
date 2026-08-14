//! Indexer coverage for PHP and Ruby.
//!
//! Asserts that `SourceParser::parse` produces the expected semantic chunks and
//! that `EdgeExtractor::extract` produces the expected import/call/inheritance
//! edges, mirroring the pattern established for the original 14 languages.

use inkentry_core::indexer::graph::{EdgeExtractor, EdgeKind};
use inkentry_core::indexer::parser::detect_language;
use inkentry_core::indexer::{ChunkKind, SourceParser};
use std::path::Path;

// ── fixtures ─────────────────────────────────────────────────────────────────

const PHP_SRC: &str = r#"<?php
require 'lib.php';
use App\Models\User;

function greet($name) {
    return "hi " . $name;
}

interface Greeter {
    public function greet();
}

trait Loggable {
    public function log() {}
}

class Base {}

class Service extends Base implements Greeter {
    use Loggable;

    public function greet() {
        helper();
        $this->run();
        return greet("x");
    }
}
"#;

const RUBY_SRC: &str = r#"require 'set'
require_relative 'helper'

module Util
  def self.log(msg)
    puts msg
  end
end

class Service < Base
  include Util

  def run
    do_work(1)
    Util.log("x")
  end
end

def top_level
  42
end
"#;

// ── helpers ──────────────────────────────────────────────────────────────────

fn names_of(chunks: &[inkentry_core::indexer::Chunk], kind: ChunkKind) -> Vec<String> {
    let want = kind.to_string();
    chunks
        .iter()
        .filter(|c| c.kind.to_string() == want)
        .filter_map(|c| c.name.clone())
        .collect()
}

fn has_edge(edges: &[inkentry_core::indexer::graph::Edge], target: &str, kind: EdgeKind) -> bool {
    edges
        .iter()
        .any(|e| e.target_name == target && e.kind == kind)
}

// ── extension detection ────────────────────────────────────────────────────

#[test]
fn detects_php_and_ruby_extensions() {
    assert_eq!(detect_language(Path::new("index.php")), Some("php"));
    assert_eq!(detect_language(Path::new("view.phtml")), Some("php"));
    assert_eq!(detect_language(Path::new("app.rb")), Some("ruby"));
    assert_eq!(detect_language(Path::new("Rakefile.rake")), Some("ruby"));
    assert_eq!(detect_language(Path::new("gem.gemspec")), Some("ruby"));
}

// ── PHP chunks ─────────────────────────────────────────────────────────────

#[test]
fn php_chunks_functions_classes_interfaces_traits() {
    let chunks = SourceParser::parse(PHP_SRC, "svc.php", "php").unwrap();

    assert!(
        names_of(&chunks, ChunkKind::Function).contains(&"greet".to_string()),
        "expected top-level function `greet`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Class).contains(&"Service".to_string()),
        "expected class `Service`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Class).contains(&"Base".to_string()),
        "expected class `Base`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Interface).contains(&"Greeter".to_string()),
        "expected interface `Greeter`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Trait).contains(&"Loggable".to_string()),
        "expected trait `Loggable`"
    );
    // The class method `greet` is captured as a Method chunk.
    assert!(
        names_of(&chunks, ChunkKind::Method).contains(&"greet".to_string()),
        "expected method `greet`"
    );
}

// ── PHP edges ──────────────────────────────────────────────────────────────

#[test]
fn php_edges_imports_calls_inheritance() {
    let edges = EdgeExtractor::extract(PHP_SRC, "svc.php", "php").unwrap();

    // require 'lib.php'
    assert!(
        has_edge(&edges, "lib.php", EdgeKind::Imports),
        "expected require import of lib.php"
    );
    // use App\Models\User;
    assert!(
        has_edge(&edges, "App\\Models\\User", EdgeKind::Imports),
        "expected namespace use import"
    );
    // helper() and greet() calls (builtins are skipped, these are user fns)
    assert!(
        has_edge(&edges, "helper", EdgeKind::Calls),
        "expected call to helper()"
    );
    assert!(
        has_edge(&edges, "greet", EdgeKind::Calls),
        "expected call to greet()"
    );
    // $this->run() member call
    assert!(
        has_edge(&edges, "run", EdgeKind::Calls),
        "expected member call run()"
    );
    // class Service extends Base implements Greeter
    assert!(
        has_edge(&edges, "Base", EdgeKind::Extends),
        "expected extends Base"
    );
    assert!(
        has_edge(&edges, "Greeter", EdgeKind::Implements),
        "expected implements Greeter"
    );
}

// ── Ruby chunks ────────────────────────────────────────────────────────────

#[test]
fn ruby_chunks_methods_classes_modules() {
    let chunks = SourceParser::parse(RUBY_SRC, "svc.rb", "ruby").unwrap();

    assert!(
        names_of(&chunks, ChunkKind::Module).contains(&"Util".to_string()),
        "expected module `Util`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Class).contains(&"Service".to_string()),
        "expected class `Service`"
    );
    let methods = names_of(&chunks, ChunkKind::Method);
    assert!(
        methods.contains(&"run".to_string()),
        "expected method `run`"
    );
    assert!(
        methods.contains(&"top_level".to_string()),
        "expected top-level def `top_level`"
    );
    // `def self.log` is a singleton_method → captured with name `log`.
    assert!(
        methods.contains(&"log".to_string()),
        "expected method `log`"
    );
}

// ── Ruby edges ─────────────────────────────────────────────────────────────

#[test]
fn ruby_edges_requires_calls_mixins_inheritance() {
    let edges = EdgeExtractor::extract(RUBY_SRC, "svc.rb", "ruby").unwrap();

    // require 'set' / require_relative 'helper'
    assert!(
        has_edge(&edges, "set", EdgeKind::Imports),
        "expected require import of set"
    );
    assert!(
        has_edge(&edges, "helper", EdgeKind::Imports),
        "expected require_relative import of helper"
    );
    // include Util → mixin (Implements)
    assert!(
        has_edge(&edges, "Util", EdgeKind::Implements),
        "expected include Util mixin edge"
    );
    // class Service < Base
    assert!(
        has_edge(&edges, "Base", EdgeKind::Extends),
        "expected extends Base"
    );
    // do_work(1) and Util.log("x") calls (puts is a builtin → skipped)
    assert!(
        has_edge(&edges, "do_work", EdgeKind::Calls),
        "expected call to do_work"
    );
    assert!(
        has_edge(&edges, "log", EdgeKind::Calls),
        "expected call to Util.log"
    );
    assert!(
        !has_edge(&edges, "puts", EdgeKind::Calls),
        "puts is a builtin and should be skipped"
    );
}
