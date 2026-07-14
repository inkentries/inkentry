//! Indexer coverage for C#, Kotlin, and Swift.
//!
//! Asserts that `SourceParser::parse` produces the expected semantic chunks and
//! that `EdgeExtractor::extract` produces the expected import/call/inheritance
//! edges, mirroring the pattern established for the original 14 languages and the
//! batch-1 php/ruby coverage.

use spelunk_core::indexer::graph::{EdgeExtractor, EdgeKind};
use spelunk_core::indexer::parser::detect_language;
use spelunk_core::indexer::{ChunkKind, SourceParser};
use std::path::Path;

// ── fixtures ─────────────────────────────────────────────────────────────────

const CSHARP_SRC: &str = r#"using System;
using System.Collections.Generic;

namespace Demo {
    public interface IGreeter { void Greet(); }

    public abstract class Base { }

    public class Service : Base, IGreeter {
        public Service() { }
        public string Name { get; set; }
        public void Greet() {
            Helper();
            this.Run();
            Console.WriteLine("hi");
        }
        private void Run() { }
    }

    public struct Point { public int X; }
    public enum Color { Red, Green }
    public record Pair(int A, int B);
}
"#;

const KOTLIN_SRC: &str = r#"package com.demo

import kotlin.collections.List
import com.demo.util.Helper

interface Greeter { fun greet() }

open class Base

class Service(val name: String) : Base(), Greeter {
    override fun greet() {
        helper()
        process()
        println("hi")
    }
    fun process() {}

    companion object {
        fun create(): Service = Service("x")
    }
}

object Singleton {
    fun ping() {}
}

fun topLevel(): Int = 42
"#;

const SWIFT_SRC: &str = r#"import Foundation
import UIKit

protocol Greeter {
    func greet()
}

class Base {}

class Service: Base, Greeter {
    var name: String = ""
    init(name: String) { self.name = name }
    func greet() {
        helper()
        self.run()
        print("hi")
    }
    func run() {}
}

struct Point {
    var x: Int
}

enum Color {
    case red, green
}

extension Service {
    func extra() {}
}

func topLevel() -> Int { return 42 }
"#;

// ── helpers ──────────────────────────────────────────────────────────────────

fn names_of(chunks: &[spelunk_core::indexer::Chunk], kind: ChunkKind) -> Vec<String> {
    let want = kind.to_string();
    chunks
        .iter()
        .filter(|c| c.kind.to_string() == want)
        .filter_map(|c| c.name.clone())
        .collect()
}

fn has_edge(edges: &[spelunk_core::indexer::graph::Edge], target: &str, kind: EdgeKind) -> bool {
    edges
        .iter()
        .any(|e| e.target_name == target && e.kind == kind)
}

// ── extension detection ────────────────────────────────────────────────────

#[test]
fn detects_csharp_kotlin_swift_extensions() {
    assert_eq!(detect_language(Path::new("Service.cs")), Some("csharp"));
    assert_eq!(detect_language(Path::new("Service.kt")), Some("kotlin"));
    assert_eq!(detect_language(Path::new("build.kts")), Some("kotlin"));
    assert_eq!(detect_language(Path::new("Service.swift")), Some("swift"));
}

// ── C# chunks ────────────────────────────────────────────────────────────────

#[test]
fn csharp_chunks_types_and_members() {
    let chunks = SourceParser::parse(CSHARP_SRC, "Service.cs", "csharp").unwrap();

    assert!(
        names_of(&chunks, ChunkKind::Class).contains(&"Service".to_string()),
        "expected class `Service`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Class).contains(&"Base".to_string()),
        "expected class `Base`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Interface).contains(&"IGreeter".to_string()),
        "expected interface `IGreeter`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Struct).contains(&"Point".to_string()),
        "expected struct `Point`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Struct).contains(&"Pair".to_string()),
        "expected record `Pair` (mapped to Struct)"
    );
    assert!(
        names_of(&chunks, ChunkKind::Enum).contains(&"Color".to_string()),
        "expected enum `Color`"
    );
    let methods = names_of(&chunks, ChunkKind::Method);
    assert!(
        methods.contains(&"Greet".to_string()),
        "expected method `Greet`"
    );
    assert!(
        methods.contains(&"Run".to_string()),
        "expected method `Run`"
    );
    // Constructor is captured as a Method named after the type.
    assert!(
        methods.contains(&"Service".to_string()),
        "expected constructor `Service`"
    );
}

// ── C# edges ─────────────────────────────────────────────────────────────────

#[test]
fn csharp_edges_usings_calls_inheritance() {
    let edges = EdgeExtractor::extract(CSHARP_SRC, "Service.cs", "csharp").unwrap();

    assert!(
        has_edge(&edges, "System", EdgeKind::Imports),
        "expected using System"
    );
    assert!(
        has_edge(&edges, "System.Collections.Generic", EdgeKind::Imports),
        "expected using System.Collections.Generic"
    );
    // Helper() and this.Run() — user calls (Console.WriteLine is a builtin, skipped).
    assert!(
        has_edge(&edges, "Helper", EdgeKind::Calls),
        "expected call to Helper()"
    );
    assert!(
        has_edge(&edges, "Run", EdgeKind::Calls),
        "expected member call Run()"
    );
    assert!(
        !has_edge(&edges, "WriteLine", EdgeKind::Calls),
        "Console.WriteLine is a builtin and should be skipped"
    );
    // class Service : Base, IGreeter — both modelled as Extends (grammar does not
    // distinguish base class from interface).
    assert!(
        has_edge(&edges, "Base", EdgeKind::Extends),
        "expected extends Base"
    );
    assert!(
        has_edge(&edges, "IGreeter", EdgeKind::Extends),
        "expected supertype IGreeter"
    );
}

// ── Kotlin chunks ──────────────────────────────────────────────────────────

#[test]
fn kotlin_chunks_classes_objects_functions() {
    let chunks = SourceParser::parse(KOTLIN_SRC, "Service.kt", "kotlin").unwrap();

    let classes = names_of(&chunks, ChunkKind::Class);
    assert!(
        classes.contains(&"Service".to_string()),
        "expected class `Service`"
    );
    assert!(
        classes.contains(&"Base".to_string()),
        "expected class `Base`"
    );
    assert!(
        classes.contains(&"Greeter".to_string()),
        "expected interface `Greeter` (mapped to Class)"
    );
    // Named object `Singleton` is captured; anonymous companion object is not named.
    assert!(
        classes.contains(&"Singleton".to_string()),
        "expected object `Singleton`"
    );
    let functions = names_of(&chunks, ChunkKind::Function);
    assert!(
        functions.contains(&"greet".to_string()),
        "expected fun `greet`"
    );
    assert!(
        functions.contains(&"process".to_string()),
        "expected fun `process`"
    );
    assert!(
        functions.contains(&"topLevel".to_string()),
        "expected top-level fun `topLevel`"
    );
    assert!(
        functions.contains(&"create".to_string()),
        "expected companion-object fun `create`"
    );
}

// ── Kotlin edges ─────────────────────────────────────────────────────────────

#[test]
fn kotlin_edges_imports_calls_inheritance() {
    let edges = EdgeExtractor::extract(KOTLIN_SRC, "Service.kt", "kotlin").unwrap();

    assert!(
        has_edge(&edges, "com.demo.util.Helper", EdgeKind::Imports),
        "expected import of com.demo.util.Helper"
    );
    // helper() and process() — user calls (println is a builtin, skipped).
    assert!(
        has_edge(&edges, "helper", EdgeKind::Calls),
        "expected call to helper()"
    );
    assert!(
        has_edge(&edges, "process", EdgeKind::Calls),
        "expected call to process()"
    );
    assert!(
        !has_edge(&edges, "println", EdgeKind::Calls),
        "println is a builtin and should be skipped"
    );
    // class Service : Base(), Greeter — both supertypes modelled as Extends.
    assert!(
        has_edge(&edges, "Base", EdgeKind::Extends),
        "expected supertype Base"
    );
    assert!(
        has_edge(&edges, "Greeter", EdgeKind::Extends),
        "expected supertype Greeter"
    );
}

// ── Swift chunks ─────────────────────────────────────────────────────────────

#[test]
fn swift_chunks_types_protocols_functions_inits() {
    let chunks = SourceParser::parse(SWIFT_SRC, "Service.swift", "swift").unwrap();

    let classes = names_of(&chunks, ChunkKind::Class);
    // class/struct/enum/extension all parse as `class_declaration` → Class.
    assert!(
        classes.contains(&"Service".to_string()),
        "expected class `Service`"
    );
    assert!(
        classes.contains(&"Base".to_string()),
        "expected class `Base`"
    );
    assert!(
        classes.contains(&"Point".to_string()),
        "expected struct `Point`"
    );
    assert!(
        classes.contains(&"Color".to_string()),
        "expected enum `Color`"
    );
    assert!(
        names_of(&chunks, ChunkKind::Interface).contains(&"Greeter".to_string()),
        "expected protocol `Greeter` (mapped to Interface)"
    );
    let functions = names_of(&chunks, ChunkKind::Function);
    assert!(
        functions.contains(&"greet".to_string()),
        "expected func `greet`"
    );
    assert!(
        functions.contains(&"run".to_string()),
        "expected func `run`"
    );
    assert!(
        functions.contains(&"extra".to_string()),
        "expected extension func `extra`"
    );
    assert!(
        functions.contains(&"topLevel".to_string()),
        "expected top-level func `topLevel`"
    );
    // init_declaration is captured as a Method named `init`.
    assert!(
        names_of(&chunks, ChunkKind::Method).contains(&"init".to_string()),
        "expected initializer captured as `init`"
    );
}

// ── Swift edges ──────────────────────────────────────────────────────────────

#[test]
fn swift_edges_imports_calls_inheritance() {
    let edges = EdgeExtractor::extract(SWIFT_SRC, "Service.swift", "swift").unwrap();

    assert!(
        has_edge(&edges, "Foundation", EdgeKind::Imports),
        "expected import Foundation"
    );
    assert!(
        has_edge(&edges, "UIKit", EdgeKind::Imports),
        "expected import UIKit"
    );
    // helper() and self.run() — user calls (print is a builtin, skipped).
    assert!(
        has_edge(&edges, "helper", EdgeKind::Calls),
        "expected call to helper()"
    );
    assert!(
        has_edge(&edges, "run", EdgeKind::Calls),
        "expected self.run() navigation call"
    );
    assert!(
        !has_edge(&edges, "print", EdgeKind::Calls),
        "print is a builtin and should be skipped"
    );
    // class Service: Base, Greeter — both modelled as Extends.
    assert!(
        has_edge(&edges, "Base", EdgeKind::Extends),
        "expected supertype Base"
    );
    assert!(
        has_edge(&edges, "Greeter", EdgeKind::Extends),
        "expected supertype Greeter"
    );
}
