//! Intra-file name binding: which definition a call's callee token denotes in
//! its lexical scope, read off the file's own tree with the vendored locals
//! query (`@local.scope` / `@local.definition.*`).

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::StreamingIterator;

/// The locals pass runs once per file on a tree that already parsed within
/// the parse budget; this bounds the query itself on a pathological tree.
const QUERY_BUDGET: Duration = Duration::from_secs(2);

/// What a callee resolves to within its file.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Resolution {
    /// A definition in this file, or a file-level binding a function or
    /// class expression initialises.
    SameFile,
    /// A parameter or local variable: the call cannot reach a repo definition.
    Suppress,
    /// An in-file alias of an imported name, which the edge targets instead.
    Alias(String),
    Unresolved,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DefKind {
    // Declared in ascending precedence: when one scope binds a name more than
    // once, the definition a call can actually reach wins over a plain value.
    Value,
    Import,
    Callable,
}

fn def_kind(capture: &str) -> Option<DefKind> {
    let sub = match capture.strip_prefix("local.definition") {
        Some("") => return Some(DefKind::Value),
        Some(rest) => rest.strip_prefix('.')?,
        None => return None,
    };
    Some(match sub {
        "function" | "method" | "type" | "macro" | "namespace" | "enum" => DefKind::Callable,
        "import" => DefKind::Import,
        _ => DefKind::Value,
    })
}

struct PendingDef<'t> {
    node: tree_sitter::Node<'t>,
    kind: DefKind,
    /// Captured as `@local.definition.method`.
    method: bool,
    /// An explicit `(#set! definition.<kind>.scope …)` on the pattern.
    hoist: Option<String>,
    /// The node the same match captured as `@local.scope`, if any.
    own_scope: Option<usize>,
}

struct Def {
    kind: DefKind,
    /// For a value, whether every binding of it here is initialised by a
    /// function, lambda or class expression.
    defines: bool,
    /// For an import that renames what it imports, the original name.
    aliased: Option<String>,
}

pub(super) struct FileScopes {
    /// Scope index by the id of the node that opens it; index 0 is the file.
    scope_of_node: HashMap<usize, usize>,
    parent: Vec<usize>,
    /// Scopes a nested scope's names never see: a Python class body is not
    /// enclosing for the methods defined in it.
    class_body: Vec<bool>,
    defs: HashMap<(usize, String), Def>,
    /// Where each callable this file defines can be reached by its bare
    /// name.
    callables: HashMap<String, Vec<super::visibility::Reach>>,
    /// Whether a call token can denote a variable at all. In Ruby, Java and
    /// PHP a call always names a method or function, whatever a same-named
    /// local holds.
    calls_reach_values: bool,
}

impl FileScopes {
    /// `None` when `language` has no locals query or the query overran its
    /// budget: the file's edges then stay unresolved.
    pub(super) fn analyse(tree: &tree_sitter::Tree, src: &[u8], language: &str) -> Option<Self> {
        let query = super::queries::locals_query(language)?;
        let root = tree.root_node();
        let mut scopes = FileScopes {
            scope_of_node: HashMap::from([(root.id(), 0)]),
            parent: vec![0],
            class_body: vec![false],
            defs: HashMap::new(),
            callables: HashMap::new(),
            calls_reach_values: !matches!(language, "ruby" | "java" | "php"),
        };

        let names = query.capture_names();
        let deadline = Instant::now() + QUERY_BUDGET;
        let mut overran = false;
        let mut on_progress = |_: &tree_sitter::QueryCursorState| {
            if Instant::now() >= deadline {
                overran = true;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let mut cursor = tree_sitter::QueryCursor::new();
        let options = tree_sitter::QueryCursorOptions::new().progress_callback(&mut on_progress);
        let mut matches = cursor.matches_with_options(query, root, src, options);

        // Scopes and definitions arrive interleaved in match order, so a
        // definition's scope is looked up only once every scope is known.
        let mut scope_nodes = Vec::new();
        let mut def_nodes = Vec::new();
        // tree-sitter applies the text predicates itself. The one other
        // predicate the queries use, Java's `#has-ancestor?` on import names,
        // is left unevaluated, so it can only widen what counts as an
        // unaliased import, a binding that leaves a call unresolved.
        while let Some(m) = matches.next() {
            let settings = query.property_settings(m.pattern_index);
            let own_scope = m
                .captures
                .iter()
                .find(|c| names[c.index as usize] == "local.scope")
                .map(|c| c.node.id());
            for c in m.captures {
                let capture = names[c.index as usize];
                if capture == "local.scope" {
                    scope_nodes.push(c.node);
                } else if let Some(kind) = def_kind(capture) {
                    let key = format!("{}.scope", capture.trim_start_matches("local."));
                    let hoist = settings
                        .iter()
                        .find(|p| *p.key == *key)
                        .and_then(|p| p.value.as_deref().map(str::to_owned));
                    def_nodes.push(PendingDef {
                        node: c.node,
                        kind,
                        method: capture == "local.definition.method",
                        hoist,
                        own_scope,
                    });
                }
            }
        }
        drop(matches);
        if overran {
            return None;
        }

        scope_nodes.sort_by_key(|n| (n.start_byte(), std::cmp::Reverse(n.end_byte())));
        for node in scope_nodes {
            if scopes.scope_of_node.contains_key(&node.id()) {
                continue;
            }
            let parent = scopes.innermost(node.parent());
            scopes.scope_of_node.insert(node.id(), scopes.parent.len());
            scopes.parent.push(parent);
            scopes
                .class_body
                .push(language == "python" && node.kind() == "class_definition");
        }

        for PendingDef {
            node,
            mut kind,
            method,
            hoist,
            own_scope,
        } in def_nodes
        {
            let Ok(name) = node.utf8_text(src) else {
                continue;
            };
            // The queries type `const { f } = require('./x')` as a variable,
            // but it binds another file's export, like an import does.
            let mut aliased = None;
            if kind == DefKind::Value
                && let Some(original) = super::aliases::required_name(node, src, language)
            {
                kind = DefKind::Import;
                aliased = original;
            }
            let mut scope = scopes.innermost(Some(node));
            match hoist.as_deref() {
                Some("parent") => scope = scopes.parent[scope],
                Some("global") => scope = 0,
                // A pattern that captures a declaration as its own scope
                // (`(import_statement …) @local.scope`, a C macro, a C++
                // template function) scopes what the declaration contains,
                // not the name it declares.
                _ if kind != DefKind::Value
                    && own_scope.and_then(|id| scopes.scope_of_node.get(&id)) == Some(&scope) =>
                {
                    scope = scopes.parent[scope];
                }
                _ => {}
            }
            if kind == DefKind::Callable {
                scopes
                    .callables
                    .entry(name.to_owned())
                    .or_default()
                    .push(super::visibility::reach_of(node, method, language));
            }
            if kind == DefKind::Import && aliased.is_none() {
                aliased = super::aliases::original_name(node, src, language);
            }
            let defines =
                kind == DefKind::Value && super::initialisers::initialised_by_a_definition(node);
            let slot = scopes.defs.entry((scope, name.to_owned()));
            let def = slot.or_insert(Def {
                kind,
                aliased: None,
                defines,
            });
            if kind == DefKind::Value {
                def.defines &= defines;
            }
            if kind >= def.kind {
                def.kind = kind;
                def.aliased = aliased;
            }
        }
        Some(scopes)
    }

    fn innermost(&self, mut node: Option<tree_sitter::Node<'_>>) -> usize {
        while let Some(n) = node {
            if let Some(&scope) = self.scope_of_node.get(&n.id()) {
                return scope;
            }
            node = n.parent();
        }
        0
    }

    /// Resolve a call to `name`. `bare_callee` is the callee token when the
    /// call is unqualified and receiver-less, the one shape a lexical binding
    /// can decide; any other call can only match a definition by name.
    pub(super) fn resolve(
        &self,
        bare_callee: Option<tree_sitter::Node<'_>>,
        name: &str,
    ) -> Resolution {
        if let Some(node) = bare_callee
            && let Some((scope, def)) = self.binding(node, name)
        {
            match def.kind {
                DefKind::Callable => return self.reached_by_bare_name(node, name),
                DefKind::Import => {
                    return match &def.aliased {
                        Some(original) => Resolution::Alias(original.clone()),
                        None => Resolution::Unresolved,
                    };
                }
                // A nested value cannot be a repo definition. A file-level one
                // is a definition here only when a function or class
                // expression initialises it (`const f = () => …`); anything
                // else (`f = other.f`, `f = make()`) could be another file's.
                DefKind::Value if scope != 0 => return Resolution::Suppress,
                DefKind::Value if def.defines => return Resolution::SameFile,
                DefKind::Value => return Resolution::Unresolved,
            }
        }
        match bare_callee {
            // A bare call no scope binds may still reach a definition the
            // query scoped too narrowly (a JS function declaration is hoisted
            // to the scope around it), but only where that name is visible.
            Some(node) => self.reached_by_bare_name(node, name),
            None if self.callables.contains_key(name) => Resolution::SameFile,
            None => Resolution::Unresolved,
        }
    }

    fn reached_by_bare_name(&self, call: tree_sitter::Node<'_>, name: &str) -> Resolution {
        let reached = self
            .callables
            .get(name)
            .is_some_and(|reaches| reaches.iter().any(|r| r.covers(call)));
        if reached {
            Resolution::SameFile
        } else {
            Resolution::Unresolved
        }
    }

    fn binding(&self, node: tree_sitter::Node<'_>, name: &str) -> Option<(usize, &Def)> {
        let start = self.innermost(Some(node));
        let mut scope = start;
        loop {
            if (scope == start || !self.class_body[scope])
                && let Some(def) = self.defs.get(&(scope, name.to_owned()))
                && (self.calls_reach_values || def.kind != DefKind::Value)
            {
                return Some((scope, def));
            }
            if scope == 0 {
                return None;
            }
            scope = self.parent[scope];
        }
    }
}
