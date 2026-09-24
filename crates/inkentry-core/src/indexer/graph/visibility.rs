//! Where a definition is reachable by its bare name: the rule a call must
//! meet before the file that defines a callable can claim it.

use tree_sitter::Node;

/// Declarations that open a function body.
const FUNCTION_KINDS: &[&str] = &[
    "function_item",
    "function_definition",
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "method_declaration",
    "constructor_declaration",
    "local_function_statement",
    "method",
    "singleton_method",
    "arrow_function",
    "function_expression",
    "func_literal",
    "closure_expression",
    "lambda_expression",
    "lambda_literal",
];

/// Declarations whose body holds members rather than statements.
pub(super) const TYPE_KINDS: &[&str] = &[
    "class_definition",
    "class_declaration",
    "class",
    "class_specifier",
    "struct_specifier",
    "struct_declaration",
    "record_declaration",
    "interface_declaration",
    "enum_declaration",
    "protocol_declaration",
    "object_declaration",
    "object_literal",
    "impl_item",
    "trait_item",
    "module",
    "object",
];

/// Languages where a bare call inside a type's body reaches that type's
/// methods through an implicit receiver. Elsewhere a method needs `self`,
/// `this` or a qualified path, so a bare name never reaches one.
fn implicit_receiver(language: &str) -> bool {
    matches!(
        language,
        "java" | "csharp" | "kotlin" | "swift" | "cpp" | "ruby"
    )
}

/// The range a bare call must sit in to reach a definition.
#[derive(Clone, Copy)]
pub(super) enum Reach {
    File,
    Within(usize, usize),
    Nowhere,
}

impl Reach {
    pub(super) fn covers(self, call: Node<'_>) -> bool {
        match self {
            Reach::File => true,
            Reach::Within(start, end) => start <= call.start_byte() && call.end_byte() <= end,
            Reach::Nowhere => false,
        }
    }
}

/// `def` is the name node of a callable definition; `method` says the locals
/// query captured it as one.
pub(super) fn reach_of(def: Node<'_>, method: bool, language: &str) -> Reach {
    // An out-of-line member (`void A::run() {}`) belongs to a type this file
    // may not even contain.
    if def
        .parent()
        .is_some_and(|p| p.kind() == "qualified_identifier")
    {
        return Reach::Nowhere;
    }
    if method && !implicit_receiver(language) {
        return Reach::Nowhere;
    }
    let mut ancestor = def.parent();
    // The tree's root is the file, whatever its kind (Python's is `module`).
    while let Some(n) = ancestor.filter(|n| n.parent().is_some()) {
        // The declaration `def` names is not what contains it.
        if !names(n, def) {
            if FUNCTION_KINDS.contains(&n.kind()) {
                return Reach::Within(n.start_byte(), n.end_byte());
            }
            if TYPE_KINDS.contains(&n.kind()) {
                return if implicit_receiver(language) {
                    Reach::Within(n.start_byte(), n.end_byte())
                } else {
                    Reach::Nowhere
                };
            }
        }
        ancestor = n.parent();
    }
    Reach::File
}

/// Whether `def` is the name `declaration` declares: its `name` or
/// `declarator`, or (Kotlin, which has no such fields) a direct child.
fn names(declaration: Node<'_>, def: Node<'_>) -> bool {
    let holds = |field: &str| {
        declaration
            .child_by_field_name(field)
            .is_some_and(|n| n.start_byte() <= def.start_byte() && def.end_byte() <= n.end_byte())
    };
    def.parent().is_some_and(|p| p.id() == declaration.id()) || holds("name") || holds("declarator")
}
