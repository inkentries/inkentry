//! Whether a variable binding's initialiser is itself a definition, which is
//! what lets a call to that binding claim the file it is written in.

use tree_sitter::Node;

/// Function, lambda and class expressions across the grammars with value
/// bindings in their locals queries.
const DEFINITION_KINDS: &[&str] = &[
    "arrow_function",
    "function_expression",
    "generator_function",
    "class",
    "lambda",
    "func_literal",
    "closure_expression",
    "lambda_expression",
    "anonymous_method_expression",
    "lambda_literal",
    "anonymous_function",
];

/// True when `def`, the name a declaration binds, is initialised by a
/// function, lambda or class expression. A destructured name, or one bound
/// alongside others, has no single initialiser and is never one.
pub(super) fn initialised_by_a_definition(def: Node<'_>) -> bool {
    initialiser(def).is_some_and(|value| DEFINITION_KINDS.contains(&value.kind()))
}

fn initialiser(def: Node<'_>) -> Option<Node<'_>> {
    let parent = def.parent()?;
    let binds_def =
        |field: &str| parent.child_by_field_name(field).map(|n| n.id()) == Some(def.id());
    let value = match parent.kind() {
        // JavaScript/TypeScript carry a `value` field; C# leaves the value
        // as the declarator's last, unnamed child.
        "variable_declarator" if binds_def("name") => parent
            .child_by_field_name("value")
            .or_else(|| parent.named_child(parent.named_child_count().checked_sub(1)? as u32))
            .filter(|v| v.id() != def.id())?,
        "assignment" if binds_def("left") => parent.child_by_field_name("right")?,
        "var_spec" | "const_spec" | "const_item" | "static_item" if binds_def("name") => {
            parent.child_by_field_name("value")?
        }
        "init_declarator" if binds_def("declarator") => parent.child_by_field_name("value")?,
        // Kotlin: `val f = { … }` puts the value beside the declaration.
        "variable_declaration" if parent.parent()?.kind() == "property_declaration" => {
            parent.next_named_sibling()?
        }
        _ => return None,
    };
    if value.kind() == "expression_list" {
        return (value.named_child_count() == 1)
            .then(|| value.named_child(0))
            .flatten();
    }
    Some(value)
}
