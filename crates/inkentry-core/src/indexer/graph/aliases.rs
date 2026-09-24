//! The imported name behind an in-file alias (`import { foo as bar }`), for
//! the grammars whose locals query binds the alias as an import definition.

use tree_sitter::Node;

/// `Some(original)` when `def` is the alias half of a renaming import.
pub(super) fn original_name(def: Node<'_>, src: &[u8], language: &str) -> Option<String> {
    let parent = def.parent()?;
    let is_alias = parent.child_by_field_name("alias").map(|n| n.id()) == Some(def.id());
    let original = match (language, parent.kind()) {
        ("javascript" | "jsx" | "typescript" | "tsx", "import_specifier") if is_alias => {
            parent.child_by_field_name("name")?
        }
        ("python", "aliased_import") if is_alias => parent.child_by_field_name("name")?,
        ("rust", "use_as_clause") if is_alias => {
            let path = parent.child_by_field_name("path")?;
            path.child_by_field_name("name").unwrap_or(path)
        }
        ("kotlin", "import_alias") => {
            let header = parent.parent()?;
            let mut cursor = header.walk();
            let path = header
                .named_children(&mut cursor)
                .find(|n| n.kind() == "identifier")?;
            let mut cursor = path.walk();
            path.named_children(&mut cursor).last()?
        }
        _ => return None,
    };
    let text = original.utf8_text(src).ok()?;
    // A dotted import path (`from a import b.c as d`) names its last segment.
    Some(text.rsplit(['.', ':']).next()?.to_owned())
}

/// For a JavaScript/TypeScript binding declared by `require(...)`, `Some` of
/// the name it renames (`const { fetch: load } = require(…)` gives
/// `Some("fetch")`), or `Some(None)` when it keeps the required name. `None`
/// when the binding is not a `require`.
pub(super) fn required_name(def: Node<'_>, src: &[u8], language: &str) -> Option<Option<String>> {
    if !matches!(language, "javascript" | "jsx" | "typescript" | "tsx") {
        return None;
    }
    let mut node = def;
    let declarator = loop {
        let parent = node.parent()?;
        match parent.kind() {
            "variable_declarator" => break parent,
            "object_pattern" | "pair_pattern" | "array_pattern" => node = parent,
            _ => return None,
        }
    };
    if declarator.child_by_field_name("name")?.id() != node.id() {
        return None;
    }
    let value = declarator.child_by_field_name("value")?;
    let callee = value.child_by_field_name("function")?;
    if value.kind() != "call_expression" || callee.utf8_text(src).ok()? != "require" {
        return None;
    }
    let pair = def.parent()?;
    let renamed = pair.kind() == "pair_pattern"
        && pair.child_by_field_name("value").map(|v| v.id()) == Some(def.id());
    Some(if renamed {
        let key = pair.child_by_field_name("key")?;
        Some(key.utf8_text(src).ok()?.to_owned())
    } else {
        None
    })
}
