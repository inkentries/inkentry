use super::EdgeKind;
use super::builtins::*;

pub(super) fn rust_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        "use_declaration" => {
            if let Ok(text) = node.utf8_text(src) {
                let path = text
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .trim()
                    .to_owned();
                if !path.is_empty() {
                    out.push((path, EdgeKind::Imports));
                }
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                match func.kind() {
                    "identifier" => {
                        if let Ok(name) = func.utf8_text(src)
                            && !is_rust_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    // Type::method(…) — index the full form, the type, and the method.
                    "scoped_identifier" => {
                        if let Ok(full) = func.utf8_text(src)
                            && !is_rust_builtin(full)
                        {
                            out.push((full.to_owned(), EdgeKind::Calls));
                        }
                        // Emit the method name: `EdgeExtractor::extract` → `extract`
                        if let Some(name_node) = func.child_by_field_name("name")
                            && let Ok(name) = name_node.utf8_text(src)
                            && !is_rust_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                        // Emit the type/path: `EdgeExtractor::extract` → `EdgeExtractor`
                        if let Some(path_node) = func.child_by_field_name("path")
                            && let Ok(path) = path_node.utf8_text(src)
                            && !is_rust_builtin(path)
                        {
                            out.push((path.to_owned(), EdgeKind::Calls));
                        }
                    }
                    // obj.method(…) — index the method name.
                    "field_expression" => {
                        if let Some(field) = func.child_by_field_name("field")
                            && let Ok(name) = field.utf8_text(src)
                            && !is_rust_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn python_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        "import_statement" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && matches!(child.kind(), "dotted_name" | "aliased_import")
                {
                    let name_node = if child.kind() == "aliased_import" {
                        child.child_by_field_name("name")
                    } else {
                        Some(child)
                    };
                    if let Some(n) = name_node
                        && let Ok(text) = n.utf8_text(src)
                    {
                        out.push((text.to_owned(), EdgeKind::Imports));
                    }
                }
            }
        }
        "import_from_statement" => {
            if let Some(module) = node.child_by_field_name("module_name") {
                if let Ok(text) = module.utf8_text(src) {
                    out.push((text.to_owned(), EdgeKind::Imports));
                }
            } else {
                out.push((".".to_owned(), EdgeKind::Imports));
            }
        }
        "call" => {
            if let Some(func) = node.child_by_field_name("function") {
                match func.kind() {
                    "identifier" => {
                        if let Ok(name) = func.utf8_text(src)
                            && !is_python_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    // obj.method(…)
                    "attribute" => {
                        if let Some(attr) = func.child_by_field_name("attribute")
                            && let Ok(name) = attr.utf8_text(src)
                            && !is_python_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn js_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        "import_statement" => {
            if let Some(source) = node.child_by_field_name("source")
                && let Ok(text) = source.utf8_text(src)
            {
                let module = text.trim_matches('"').trim_matches('\'').to_owned();
                out.push((module, EdgeKind::Imports));
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                match func.kind() {
                    "identifier" => {
                        if let Ok(name) = func.utf8_text(src)
                            && !is_js_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    // obj.method(…)
                    "member_expression" => {
                        if let Some(prop) = func.child_by_field_name("property")
                            && let Ok(name) = prop.utf8_text(src)
                            && !is_js_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn go_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        "import_spec" => {
            if let Some(path) = node.child_by_field_name("path")
                && let Ok(text) = path.utf8_text(src)
            {
                let module = text.trim_matches('"').to_owned();
                out.push((module, EdgeKind::Imports));
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                match func.kind() {
                    "identifier" => {
                        if let Ok(name) = func.utf8_text(src)
                            && !is_go_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    "selector_expression" => {
                        if let Ok(text) = func.utf8_text(src) {
                            out.push((text.to_owned(), EdgeKind::Calls));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn java_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        "import_declaration" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && matches!(child.kind(), "scoped_identifier" | "identifier")
                {
                    if let Ok(text) = child.utf8_text(src) {
                        out.push((text.to_owned(), EdgeKind::Imports));
                    }
                    break;
                }
            }
        }
        "class_declaration" => {
            if let Some(superclass) = node.child_by_field_name("superclass")
                && let Ok(text) = superclass.utf8_text(src)
            {
                let name = text.trim_start_matches("extends").trim().to_owned();
                if !name.is_empty() {
                    out.push((name, EdgeKind::Extends));
                }
            }
            if let Some(interfaces) = node.child_by_field_name("interfaces")
                && let Ok(text) = interfaces.utf8_text(src)
            {
                for name in text.trim_start_matches("implements").trim().split(',') {
                    let n = name.trim().to_owned();
                    if !n.is_empty() {
                        out.push((n, EdgeKind::Implements));
                    }
                }
            }
        }
        "method_invocation" => {
            if let Some(name) = node.child_by_field_name("name")
                && let Ok(text) = name.utf8_text(src)
            {
                out.push((text.to_owned(), EdgeKind::Calls));
            }
        }
        _ => {}
    }
    out
}

pub(super) fn php_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        // require/require_once/include "file.php" — the path is a `string` child
        // whose text sits inside a `string_content` node.
        "require_expression" | "include_expression" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "string"
                    && let Ok(text) = child.utf8_text(src)
                {
                    let path = text.trim_matches('"').trim_matches('\'').to_owned();
                    if !path.is_empty() {
                        out.push((path, EdgeKind::Imports));
                    }
                    break;
                }
            }
        }
        // use App\Models\User; — namespace import.
        "namespace_use_declaration" => {
            if let Ok(text) = node.utf8_text(src) {
                let path = text
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .trim()
                    .to_owned();
                if !path.is_empty() {
                    out.push((path, EdgeKind::Imports));
                }
            }
        }
        // foo() — the callee is in the `function` field.
        "function_call_expression" => {
            if let Some(func) = node.child_by_field_name("function")
                && func.kind() == "name"
                && let Ok(name) = func.utf8_text(src)
                && !is_php_builtin(name)
            {
                out.push((name.to_owned(), EdgeKind::Calls));
            }
        }
        // $obj->method() / self::method() — the method name is in the `name` field.
        "member_call_expression" | "scoped_call_expression" => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(name) = name_node.utf8_text(src)
                && !is_php_builtin(name)
            {
                out.push((name.to_owned(), EdgeKind::Calls));
            }
        }
        // class C extends Base implements I, J { … }
        "class_declaration" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32) {
                    match child.kind() {
                        "base_clause" => {
                            for j in 0..child.child_count() {
                                if let Some(base) = child.child(j as u32)
                                    && base.kind() == "name"
                                    && let Ok(text) = base.utf8_text(src)
                                {
                                    out.push((text.to_owned(), EdgeKind::Extends));
                                }
                            }
                        }
                        "class_interface_clause" => {
                            for j in 0..child.child_count() {
                                if let Some(iface) = child.child(j as u32)
                                    && iface.kind() == "name"
                                    && let Ok(text) = iface.utf8_text(src)
                                {
                                    out.push((text.to_owned(), EdgeKind::Implements));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn ruby_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        // Ruby has no dedicated import/mixin syntax — require, require_relative,
        // include, extend, and ordinary calls are all `call` nodes whose callee
        // is the `method` field.  Classify by the method name.
        "call" => {
            if let Some(method) = node.child_by_field_name("method")
                && method.kind() == "identifier"
                && let Ok(name) = method.utf8_text(src)
            {
                match name {
                    "require" | "require_relative" | "load" | "autoload" => {
                        if let Some(args) = node.child_by_field_name("arguments") {
                            for i in 0..args.child_count() {
                                if let Some(arg) = args.child(i as u32)
                                    && arg.kind() == "string"
                                    && let Ok(text) = arg.utf8_text(src)
                                {
                                    let path = text.trim_matches('"').trim_matches('\'').to_owned();
                                    if !path.is_empty() {
                                        out.push((path, EdgeKind::Imports));
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    // include Mod / extend Mod / prepend Mod — mixin, modelled as Implements.
                    "include" | "extend" | "prepend" => {
                        if let Some(args) = node.child_by_field_name("arguments") {
                            for i in 0..args.child_count() {
                                if let Some(arg) = args.child(i as u32)
                                    && arg.kind() == "constant"
                                    && let Ok(text) = arg.utf8_text(src)
                                {
                                    out.push((text.to_owned(), EdgeKind::Implements));
                                }
                            }
                        }
                    }
                    other if !is_ruby_builtin(other) => {
                        out.push((other.to_owned(), EdgeKind::Calls));
                    }
                    _ => {}
                }
            }
        }
        // class Service < Base — single inheritance via the `superclass` field.
        "class" => {
            if let Some(superclass) = node.child_by_field_name("superclass") {
                // `superclass` node wraps `< Base`; the constant is its named child.
                for i in 0..superclass.child_count() {
                    if let Some(child) = superclass.child(i as u32)
                        && child.kind() == "constant"
                        && let Ok(text) = child.utf8_text(src)
                    {
                        out.push((text.to_owned(), EdgeKind::Extends));
                    }
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn csharp_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        // using System; / using System.Collections.Generic;
        "using_directive" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && matches!(child.kind(), "identifier" | "qualified_name")
                    && let Ok(text) = child.utf8_text(src)
                {
                    out.push((text.to_owned(), EdgeKind::Imports));
                    break;
                }
            }
        }
        // Foo() / obj.Method() / Type.Method() — callee is the `function` field.
        "invocation_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                match func.kind() {
                    "identifier" => {
                        if let Ok(name) = func.utf8_text(src)
                            && !is_csharp_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    // obj.Method() / Type.Method() — method name is the `name` field.
                    "member_access_expression" => {
                        if let Some(name_node) = func.child_by_field_name("name")
                            && let Ok(name) = name_node.utf8_text(src)
                            && !is_csharp_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    _ => {}
                }
            }
        }
        // class Service : Base, IGreeter { … } — the base type and interfaces are
        // listed in a `base_list`, which the grammar does not distinguish
        // syntactically, so every entry is modelled as Extends.
        "class_declaration"
        | "struct_declaration"
        | "record_declaration"
        | "interface_declaration" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "base_list"
                {
                    for j in 0..child.child_count() {
                        if let Some(base) = child.child(j as u32)
                            && matches!(
                                base.kind(),
                                "identifier" | "qualified_name" | "generic_name"
                            )
                            && let Ok(text) = base.utf8_text(src)
                        {
                            out.push((text.to_owned(), EdgeKind::Extends));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn kotlin_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        // import com.demo.util.Helper
        "import_header" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "identifier"
                    && let Ok(text) = child.utf8_text(src)
                {
                    out.push((text.to_owned(), EdgeKind::Imports));
                    break;
                }
            }
        }
        // foo() / println(x) — the callee is the leading `simple_identifier`.
        // obj.method() nests differently (navigation_expression) and is not
        // captured here, matching the conservative approach the other langs take.
        "call_expression" => {
            if let Some(callee) = node.child(0)
                && callee.kind() == "simple_identifier"
                && let Ok(name) = callee.utf8_text(src)
                && !is_kotlin_builtin(name)
            {
                out.push((name.to_owned(), EdgeKind::Calls));
            }
        }
        // class Service(…) : Base(), Greeter — supertypes are `delegation_specifier`
        // children. Kotlin does not distinguish class inheritance from interface
        // implementation syntactically, so every supertype is modelled as Extends.
        "class_declaration" | "object_declaration" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "delegation_specifier"
                    && let Some(name) = user_type_name(&child, src)
                {
                    out.push((name, EdgeKind::Extends));
                }
            }
        }
        _ => {}
    }
    out
}

pub(super) fn swift_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        // import Foundation
        "import_declaration" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "identifier"
                    && let Ok(text) = child.utf8_text(src)
                {
                    out.push((text.to_owned(), EdgeKind::Imports));
                    break;
                }
            }
        }
        // foo() / self.run() / print(x) — the callee is either a leading
        // `simple_identifier` or a `navigation_expression` (obj.method).
        "call_expression" => {
            if let Some(callee) = node.child(0) {
                match callee.kind() {
                    "simple_identifier" => {
                        if let Ok(name) = callee.utf8_text(src)
                            && !is_swift_builtin(name)
                        {
                            out.push((name.to_owned(), EdgeKind::Calls));
                        }
                    }
                    // self.run() / obj.method() — method name is the
                    // `simple_identifier` inside the trailing `navigation_suffix`.
                    "navigation_expression" => {
                        if let Some(name) = navigation_suffix_name(&callee, src)
                            && !is_swift_builtin(&name)
                        {
                            out.push((name, EdgeKind::Calls));
                        }
                    }
                    _ => {}
                }
            }
        }
        // class Service: Base, Greeter { … } — supertypes are `inheritance_specifier`
        // children. Swift does not distinguish superclass from protocol conformance
        // syntactically, so each is modelled as Extends.
        "class_declaration" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "inheritance_specifier"
                    && let Some(name) = user_type_name(&child, src)
                {
                    out.push((name, EdgeKind::Extends));
                }
            }
        }
        _ => {}
    }
    out
}

/// Extract a type name from a node wrapping a `user_type` → `type_identifier`
/// (Kotlin `delegation_specifier`, Swift `inheritance_specifier`). Falls back to
/// a directly-nested `type_identifier`.
fn user_type_name(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            match child.kind() {
                "type_identifier" => return child.utf8_text(src).ok().map(str::to_owned),
                "user_type" | "constructor_invocation" => {
                    for j in 0..child.child_count() {
                        if let Some(g) = child.child(j as u32)
                            && g.kind() == "type_identifier"
                        {
                            return g.utf8_text(src).ok().map(str::to_owned);
                        }
                        // constructor_invocation nests user_type → type_identifier
                        if let Some(g) = child.child(j as u32)
                            && g.kind() == "user_type"
                        {
                            for k in 0..g.child_count() {
                                if let Some(t) = g.child(k as u32)
                                    && t.kind() == "type_identifier"
                                {
                                    return t.utf8_text(src).ok().map(str::to_owned);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Return the method name from a Swift `navigation_expression` (`obj.method`),
/// i.e. the `simple_identifier` inside its `navigation_suffix`.
fn navigation_suffix_name(node: &tree_sitter::Node<'_>, src: &[u8]) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32)
            && child.kind() == "navigation_suffix"
        {
            for j in 0..child.child_count() {
                if let Some(g) = child.child(j as u32)
                    && g.kind() == "simple_identifier"
                {
                    return g.utf8_text(src).ok().map(str::to_owned);
                }
            }
        }
    }
    None
}

pub(super) fn c_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    match node.kind() {
        "preproc_include" => {
            if let Some(path) = node.child_by_field_name("path")
                && let Ok(text) = path.utf8_text(src)
            {
                let module = text
                    .trim_matches('"')
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned();
                out.push((module, EdgeKind::Imports));
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function")
                && func.kind() == "identifier"
                && let Ok(name) = func.utf8_text(src)
                && !is_c_builtin(name)
            {
                out.push((name.to_owned(), EdgeKind::Calls));
            }
        }
        _ => {}
    }
    out
}

pub(super) fn html_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    // tree-sitter-html uses child kinds `attribute_name` / `attribute_value`,
    // not named fields.  Walk the `attribute` node's children directly.
    if node.kind() == "attribute" {
        let mut attr_name = "";
        let mut attr_value = "";

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                match child.kind() {
                    "attribute_name" => {
                        attr_name = child.utf8_text(src).unwrap_or("");
                    }
                    "attribute_value" | "quoted_attribute_value" => {
                        attr_value = child.utf8_text(src).unwrap_or("");
                    }
                    _ => {}
                }
            }
        }

        if matches!(attr_name, "src" | "href") {
            let path = attr_value.trim_matches('"').trim_matches('\'').to_owned();
            if !path.is_empty() && !path.starts_with('#') && !path.starts_with("data:") {
                out.push((path, EdgeKind::Imports));
            }
        }
    }
    out
}

pub(super) fn css_edges(node: &tree_sitter::Node<'_>, src: &[u8]) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    // @import "file.css" or @import url("file.css")
    if node.kind() == "import_statement" {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32)
                && matches!(child.kind(), "string_value" | "call_expression")
            {
                if let Ok(text) = child.utf8_text(src) {
                    let path = text
                        .trim_start_matches("url(")
                        .trim_end_matches(')')
                        .trim_matches('"')
                        .trim_matches('\'')
                        .to_owned();
                    if !path.is_empty() {
                        out.push((path, EdgeKind::Imports));
                    }
                }
                break;
            }
        }
    }
    out
}
