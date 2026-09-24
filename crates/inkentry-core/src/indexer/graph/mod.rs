//! Structural edge extraction from source files using tree-sitter.
//!
//! Extracts `imports`, `calls`, `extends`, and `implements` edges for every
//! supported language.  The resulting [`Edge`] values are stored in the
//! `graph_edges` SQLite table and queried by `inkentry graph`.
//!
//! # Design
//! A single recursive tree walk visits every node.  Per-language helper
//! functions decide whether a given node carries an edge; the rest of the
//! traversal logic is shared.  Call edges are deduplicated per
//! (source_name, target_name, target_file) to keep the graph compact.
//!
//! Where the language has a vendored locals query, each call edge's callee is
//! also bound within the file (`locals`): an edge to a definition this file
//! makes carries it as `target_file`, and a call whose callee is a parameter
//! or local variable is dropped, since it cannot reach a repo definition.

mod aliases;
mod builtins;
mod edges;
mod initialisers;
mod locals;
mod queries;
#[cfg(test)]
mod tests;
mod visibility;

use anyhow::Result;
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Imports,
    Calls,
    Extends,
    Implements,
    /// No longer written. Kept so the rows an older index still holds parse
    /// as themselves rather than as an unknown kind.
    Mentions,
}

impl std::fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Imports => write!(f, "imports"),
            Self::Calls => write!(f, "calls"),
            Self::Extends => write!(f, "extends"),
            Self::Implements => write!(f, "implements"),
            Self::Mentions => write!(f, "mentions"),
        }
    }
}

impl EdgeKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "calls" => Self::Calls,
            "extends" => Self::Extends,
            "implements" => Self::Implements,
            "mentions" => Self::Mentions,
            _ => Self::Imports,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub source_file: String,
    /// Enclosing function/class name at the point of the edge, if known.
    pub source_name: Option<String>,
    /// Imported module path, called function, or base class name.
    pub target_name: String,
    pub kind: EdgeKind,
    /// 1-based source line where the relationship appears.
    pub line: usize,
    /// The file defining the callee this edge resolved to; `None` when
    /// unresolved, which consumers treat as "any definition named
    /// `target_name`".
    pub target_file: Option<String>,
}

/// An edge a language helper found at one node, before resolution.
pub(super) struct Candidate<'t> {
    target: String,
    kind: EdgeKind,
    /// The callee token of an unqualified, receiver-less call: the only call
    /// shape a lexical binding can decide.
    bare_callee: Option<tree_sitter::Node<'t>>,
}

impl<'t> Candidate<'t> {
    pub(super) fn edge(target: String, kind: EdgeKind) -> Self {
        Self {
            target,
            kind,
            bare_callee: None,
        }
    }

    pub(super) fn bare_call(target: String, callee: tree_sitter::Node<'t>) -> Self {
        Self {
            target,
            kind: EdgeKind::Calls,
            bare_callee: Some(callee),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct EdgeExtractor;

impl EdgeExtractor {
    /// Extract all structural edges from `source`, resolving call targets
    /// within the file. Returns an empty vec on parse failure rather than an
    /// error.
    pub fn extract(source: &str, file_path: &str, language: &str) -> Result<Vec<Edge>> {
        Self::run(source, file_path, language, true)
    }

    /// [`extract`](Self::extract) without the locals pass, for a file the
    /// chunker handled as a sliding window rather than by its tree: every
    /// edge stays unresolved.
    pub fn extract_unresolved(source: &str, file_path: &str, language: &str) -> Result<Vec<Edge>> {
        Self::run(source, file_path, language, false)
    }

    fn run(source: &str, file_path: &str, language: &str, resolve: bool) -> Result<Vec<Edge>> {
        let ts_lang = match super::parser::ts_language_pub(language) {
            Ok(l) => l,
            Err(_) => return Ok(vec![]),
        };

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang)?;

        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return Ok(vec![]),
        };

        let bytes = source.as_bytes();
        // Over the parse cap the chunker never uses the tree, so neither
        // does resolution, whoever the caller is.
        let scopes = (resolve && source.len() <= super::parser::MAX_PARSE_BYTES)
            .then(|| locals::FileScopes::analyse(&tree, bytes, language))
            .flatten();
        let ctx = Ctx {
            src: bytes,
            file_path,
            language,
            scopes: scopes.as_ref(),
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();

        walk(tree.root_node(), &ctx, None, &mut out, &mut seen);
        Ok(out)
    }
}

struct Ctx<'a> {
    src: &'a [u8],
    file_path: &'a str,
    language: &'a str,
    scopes: Option<&'a locals::FileScopes>,
}

type SeenKey = (Option<String>, String, String, Option<String>);

// ---------------------------------------------------------------------------
// Tree walker
// ---------------------------------------------------------------------------

fn walk(
    node: tree_sitter::Node<'_>,
    ctx: &Ctx<'_>,
    enclosing: Option<&str>,
    out: &mut Vec<Edge>,
    seen: &mut HashSet<SeenKey>,
) {
    // Track the enclosing function/class as we descend.
    let new_scope = enclosing_scope(&node, ctx.src, ctx.language);
    let eff = new_scope.as_deref().or(enclosing);

    collect(&node, ctx, eff, out, seen);

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk(child, ctx, eff, out, seen);
        }
    }
}

/// If `node` introduces a named scope (function, class, …) return its name.
fn enclosing_scope(node: &tree_sitter::Node<'_>, src: &[u8], language: &str) -> Option<String> {
    let field = match (language, node.kind()) {
        ("rust", "function_item") => "name",
        ("python", "function_definition") => "name",
        ("python", "class_definition") => "name",
        ("javascript" | "jsx" | "typescript" | "tsx", "function_declaration") => "name",
        ("javascript" | "jsx" | "typescript" | "tsx", "class_declaration") => "name",
        ("go", "function_declaration") => "name",
        ("go", "method_declaration") => "name",
        ("java", "class_declaration") => "name",
        ("java", "method_declaration") => "name",
        ("php", "function_definition") => "name",
        ("php", "method_declaration") => "name",
        ("php", "class_declaration") => "name",
        ("ruby", "method") => "name",
        ("ruby", "singleton_method") => "name",
        ("ruby", "class") => "name",
        ("ruby", "module") => "name",
        // C# exposes a direct `name` field on each declaration.
        ("csharp", "class_declaration") => "name",
        ("csharp", "struct_declaration") => "name",
        ("csharp", "interface_declaration") => "name",
        ("csharp", "record_declaration") => "name",
        ("csharp", "method_declaration") => "name",
        ("csharp", "constructor_declaration") => "name",
        // Swift exposes a `name` field on class/struct/enum/extension
        // (`class_declaration`), protocols, and functions.
        ("swift", "class_declaration") => "name",
        ("swift", "protocol_declaration") => "name",
        ("swift", "function_declaration") => "name",
        // Kotlin has no `name` field — its scopes are resolved below.
        ("kotlin", "class_declaration" | "object_declaration") => {
            return kotlin_scope_name(node, src, "type_identifier");
        }
        ("kotlin", "function_declaration") => {
            return kotlin_scope_name(node, src, "simple_identifier");
        }
        _ => return None,
    };
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(src).ok())
        .map(str::to_owned)
}

/// Resolve a Kotlin scope name from an unnamed child of the given kind
/// (`type_identifier` for classes/objects, `simple_identifier` for functions).
fn kotlin_scope_name(node: &tree_sitter::Node<'_>, src: &[u8], child_kind: &str) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32)
            && child.kind() == child_kind
        {
            return child.utf8_text(src).ok().map(str::to_owned);
        }
    }
    None
}

/// Emit edges (if any) produced by `node`, deduplicating via `seen`.
fn collect(
    node: &tree_sitter::Node<'_>,
    ctx: &Ctx<'_>,
    enclosing: Option<&str>,
    out: &mut Vec<Edge>,
    seen: &mut HashSet<SeenKey>,
) {
    let line = node.start_position().row + 1;
    let src = ctx.src;

    let candidates: Vec<Candidate<'_>> = match ctx.language {
        "rust" => edges::rust_edges(node, src),
        "python" => edges::python_edges(node, src),
        "javascript" | "jsx" | "typescript" | "tsx" => edges::js_edges(node, src),
        "go" => edges::go_edges(node, src),
        "java" => edges::java_edges(node, src),
        "c" | "cpp" => edges::c_edges(node, src),
        "php" => edges::php_edges(node, src),
        "ruby" => edges::ruby_edges(node, src),
        "csharp" => edges::csharp_edges(node, src),
        "kotlin" => edges::kotlin_edges(node, src),
        "swift" => edges::swift_edges(node, src),
        "html" => edges::html_edges(node, src),
        "css" => edges::css_edges(node, src),
        _ => vec![],
    };

    for candidate in candidates {
        let Candidate {
            mut target,
            kind,
            bare_callee,
        } = candidate;
        let mut target_file = None;
        if kind == EdgeKind::Calls
            && let Some(scopes) = ctx.scopes
        {
            match scopes.resolve(bare_callee, &target) {
                locals::Resolution::SameFile => target_file = Some(ctx.file_path.to_owned()),
                locals::Resolution::Suppress => continue,
                locals::Resolution::Alias(original) => target = original,
                locals::Resolution::Unresolved => {}
            }
        }
        let key = (
            enclosing.map(str::to_owned),
            target.clone(),
            kind.to_string(),
            target_file.clone(),
        );
        if seen.insert(key) {
            out.push(Edge {
                source_file: ctx.file_path.to_owned(),
                source_name: enclosing.map(str::to_owned),
                target_name: target,
                kind,
                line,
                target_file,
            });
        }
    }
}
