//! The receiver calls a file can place: through the enclosing type's own
//! self-reference, or through a qualifier that statically names a type or
//! module of this file. Any other receiver's type decides which method runs.

use std::collections::HashMap;

use tree_sitter::Node;

/// What a call's receiver or qualifier names.
pub(super) enum Receiver<'t> {
    /// The enclosing type's own self-reference: the type's key. For Go, also
    /// the method whose receiver it must still be.
    SelfOf {
        owner: String,
        go_method: Option<Node<'t>>,
    },
    /// A bare type name, which the file must define and not shadow.
    TypeName(&'t str),
    /// Rust `self::`, the current module: a file-scope function.
    Module,
    Other,
}

pub(super) fn classify<'t>(receiver: Node<'t>, src: &'t [u8], language: &str) -> Receiver<'t> {
    let Ok(text) = receiver.utf8_text(src) else {
        return Receiver::Other;
    };
    let self_of = |go_method| match owner(receiver, src, language) {
        Some(owner) => Receiver::SelfOf { owner, go_method },
        None => Receiver::Other,
    };
    match language {
        "javascript" | "jsx" | "typescript" | "tsx" if text == "this" => {
            if this_rebound_in_js(receiver) {
                Receiver::Other
            } else {
                self_of(None)
            }
        }
        "java" | "csharp" if text == "this" => {
            if inside_anonymous_class(receiver, language) {
                Receiver::Other
            } else {
                self_of(None)
            }
        }
        "python" if matches!(text, "self" | "cls") => {
            match python_method_binding(receiver, text, src) {
                Some(method) => match owner(method, src, language) {
                    Some(owner) => Receiver::SelfOf {
                        owner,
                        go_method: None,
                    },
                    None => Receiver::Other,
                },
                None => Receiver::Other,
            }
        }
        "php" if text == "$this" || (receiver.kind() == "relative_scope" && text == "self") => {
            self_of(None)
        }
        "rust"
            if receiver.kind() == "self"
                && receiver
                    .parent()
                    .is_some_and(|p| p.kind() == "scoped_identifier") =>
        {
            Receiver::Module
        }
        "rust" | "swift" | "ruby" if matches!(text, "self" | "Self") => self_of(None),
        "go" => match enclosing(receiver, &["method_declaration"]) {
            Some(method)
                if go_receiver(method)
                    .and_then(|p| p.child_by_field_name("name"))
                    .and_then(|n| n.utf8_text(src).ok())
                    == Some(text) =>
            {
                self_of(Some(method))
            }
            _ => Receiver::Other,
        },
        "rust" | "php" | "python" | "javascript" | "jsx" | "typescript" | "tsx" | "java"
            if matches!(receiver.kind(), "identifier" | "name" | "type_identifier") =>
        {
            Receiver::TypeName(text)
        }
        _ => Receiver::Other,
    }
}

/// The type whose body `node` sits in, as a key that matches across the
/// several places one type's methods can be written (Rust `impl` blocks, Go
/// methods, which sit at file level and name their type in the receiver).
pub(super) fn owner(node: Node<'_>, src: &[u8], language: &str) -> Option<String> {
    let text = |n: Node<'_>| n.utf8_text(src).ok().map(str::to_owned);
    let mut ancestor = node.parent();
    while let Some(n) = ancestor {
        match n.kind() {
            "method_declaration" if language == "go" => {
                let mut ty = go_receiver(n)?.child_by_field_name("type")?;
                if ty.kind() == "pointer_type" {
                    ty = ty.named_child(0)?;
                }
                return text(ty);
            }
            "impl_item" => return text(n.child_by_field_name("type")?),
            kind if super::visibility::TYPE_KINDS.contains(&kind) => {
                return Some(match n.child_by_field_name("name") {
                    Some(name) => text(name)?,
                    None => format!("@{}", n.start_byte()),
                });
            }
            _ => {}
        }
        ancestor = n.parent();
    }
    None
}

/// The types declared at the top of the file, by name, keyed like [`owner`].
pub(super) fn file_types(root: Node<'_>, src: &[u8]) -> HashMap<String, String> {
    const DECLARATIONS: &[&str] = &[
        "class_definition",
        "class_declaration",
        "abstract_class_declaration",
        "interface_declaration",
        "enum_declaration",
        "record_declaration",
        "struct_item",
        "enum_item",
        "union_item",
        "trait_item",
    ];
    let mut types = HashMap::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let declaration = match child.kind() {
            "export_statement" => child.child_by_field_name("declaration"),
            "decorated_definition" => child.child_by_field_name("definition"),
            _ => Some(child),
        };
        if let Some(d) = declaration.filter(|d| DECLARATIONS.contains(&d.kind()))
            && let Some(name) = d.child_by_field_name("name")
            && let Ok(name) = name.utf8_text(src)
        {
            types.insert(name.to_owned(), name.to_owned());
        }
    }
    types
}

fn enclosing<'t>(node: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
    let mut ancestor = node.parent();
    while let Some(n) = ancestor {
        if kinds.contains(&n.kind()) {
            return Some(n);
        }
        ancestor = n.parent();
    }
    None
}

/// A non-arrow function between `this` and its method binds its own `this`.
fn this_rebound_in_js(this: Node<'_>) -> bool {
    let mut ancestor = this.parent();
    while let Some(n) = ancestor {
        match n.kind() {
            "function_expression"
            | "function_declaration"
            | "function"
            | "generator_function"
            | "generator_function_declaration" => return true,
            "method_definition" | "field_definition" | "public_field_definition" => return false,
            _ => {}
        }
        ancestor = n.parent();
    }
    false
}

/// `this` in an anonymous class body means the anonymous type.
fn inside_anonymous_class(this: Node<'_>, language: &str) -> bool {
    language == "java"
        && enclosing(this, &["class_body", "class_declaration"]).is_some_and(|body| {
            body.kind() == "class_body"
                && body
                    .parent()
                    .is_some_and(|p| p.kind() == "object_creation_expression")
        })
}

/// The method whose own `self`/`cls` parameter this receiver still is: the
/// nearest function or lambda declaring that name, provided it is a method
/// of a class and nothing in it reassigns the name.
fn python_method_binding<'t>(receiver: Node<'t>, name: &str, src: &[u8]) -> Option<Node<'t>> {
    let mut ancestor = receiver.parent();
    while let Some(n) = ancestor {
        if matches!(n.kind(), "function_definition" | "lambda") && declares(n, name, src) {
            let method = n.kind() == "function_definition" && is_python_method(n);
            return (method && !reassigns(n, name, src)).then_some(n);
        }
        ancestor = n.parent();
    }
    None
}

fn declares(function: Node<'_>, name: &str, src: &[u8]) -> bool {
    let Some(params) = function.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = params.walk();
    params.named_children(&mut cursor).any(|p| {
        let id = if p.kind() == "identifier" {
            Some(p)
        } else {
            p.child_by_field_name("name").or_else(|| p.named_child(0))
        };
        id.and_then(|id| id.utf8_text(src).ok()) == Some(name)
    })
}

fn is_python_method(function: Node<'_>) -> bool {
    let mut parent = function.parent();
    if parent.is_some_and(|p| p.kind() == "decorated_definition") {
        parent = parent.and_then(|p| p.parent());
    }
    parent.is_some_and(|block| {
        block.kind() == "block"
            && block
                .parent()
                .is_some_and(|c| c.kind() == "class_definition")
    })
}

fn reassigns(function: Node<'_>, name: &str, src: &[u8]) -> bool {
    let Some(body) = function.child_by_field_name("body") else {
        return false;
    };
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "function_definition" | "lambda" | "class_definition" => continue,
            "assignment" | "augmented_assignment" | "named_expression" => {
                let target = n
                    .child_by_field_name("left")
                    .or_else(|| n.child_by_field_name("name"));
                if target.and_then(|t| t.utf8_text(src).ok()) == Some(name) {
                    return true;
                }
            }
            _ => {}
        }
        let mut cursor = n.walk();
        stack.extend(n.named_children(&mut cursor));
    }
    false
}

fn go_receiver(method: Node<'_>) -> Option<Node<'_>> {
    let list = method.child_by_field_name("receiver")?;
    let mut cursor = list.walk();
    list.named_children(&mut cursor)
        .find(|c| c.kind() == "parameter_declaration")
}
