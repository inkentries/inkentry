// A function bound to a module-level `const`/`let`/`var` is how most JS/TS
// functions and nearly all React components are written. Without a chunk of
// its own, a file that also held an interface or type lost the component's
// lines from the index entirely.

use inkentry_core::indexer::{Chunk, SourceParser};

fn names(chunks: &[Chunk]) -> Vec<(&str, String)> {
    chunks
        .iter()
        .filter_map(|c| c.name.as_deref().map(|n| (n, c.kind.to_string())))
        .collect()
}

fn function(name: &str) -> (&str, String) {
    (name, "function".to_owned())
}

const COMPONENT_TSX: &str = r#"import { memo, forwardRef } from 'react'

type PickerProps = { value: string }

const MAX_SIZE = 800000
const PUBLIC_PATHS = ['/login', '/signup']

export const Picker = ({ value }: PickerProps) => {
  const onClick = () => console.log(value)
  return <button onClick={onClick}>{value}</button>
}

export const Badge = memo(({ label }: { label: string }) => <span>{label}</span>)

export const Input = forwardRef<HTMLInputElement, PickerProps>((props, ref) => (
  <input ref={ref} value={props.value} />
))

const formatAmount = function (cents: number) {
  return (cents / 100).toFixed(2)
}
"#;

#[test]
fn a_module_level_arrow_component_is_its_own_named_function_chunk() {
    let chunks = SourceParser::parse(COMPONENT_TSX, "Picker.tsx", "tsx").unwrap();
    let got = names(&chunks);
    for expected in ["Picker", "Badge", "Input", "formatAmount"] {
        assert!(
            got.contains(&function(expected)),
            "{expected} must be a named function chunk: {got:?}"
        );
    }
    let picker = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("Picker"))
        .unwrap();
    assert!(
        picker.content.contains("return <button"),
        "{}",
        picker.content
    );
}

#[test]
fn a_binding_that_is_not_a_function_is_not_a_chunk() {
    let chunks = SourceParser::parse(COMPONENT_TSX, "Picker.tsx", "tsx").unwrap();
    let got = names(&chunks);
    for not_expected in ["MAX_SIZE", "PUBLIC_PATHS"] {
        assert!(
            !got.iter().any(|(n, _)| *n == not_expected),
            "{not_expected} is not a function: {got:?}"
        );
    }
}

#[test]
fn a_function_bound_inside_a_component_stays_part_of_it() {
    let chunks = SourceParser::parse(COMPONENT_TSX, "Picker.tsx", "tsx").unwrap();
    assert!(
        !chunks.iter().any(|c| c.name.as_deref() == Some("onClick")),
        "onClick is already inside Picker's chunk"
    );
}

#[test]
fn plain_javascript_and_typescript_bindings_are_chunked_too() {
    let js = "export const add = (a, b) => a + b\nvar legacy = function () { return 1 }\n";
    for (path, lang) in [
        ("m.js", "javascript"),
        ("m.jsx", "jsx"),
        ("m.ts", "typescript"),
    ] {
        let chunks = SourceParser::parse(js, path, lang).unwrap();
        let got = names(&chunks);
        assert!(got.contains(&function("add")), "{lang}: {got:?}");
        assert!(got.contains(&function("legacy")), "{lang}: {got:?}");
    }
}

#[test]
fn a_doc_comment_above_an_exported_binding_is_its_docstring() {
    let src = "/** Formats cents as a currency amount. */\nexport const format = (c: number) => `${c / 100}`\n";
    let chunks = SourceParser::parse(src, "format.ts", "typescript").unwrap();
    let format = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("format"))
        .unwrap();
    assert!(
        format
            .docstring
            .as_deref()
            .is_some_and(|d| d.contains("Formats cents")),
        "{:?}",
        format.docstring
    );
}

#[test]
fn a_doc_comment_above_an_exported_declaration_is_its_docstring() {
    let src = "/** Greets a user. */\nexport function greet(n: string) { return n }\n\n\
               /** Props for a button. */\nexport interface ButtonProps { label: string }\n";
    let chunks = SourceParser::parse(src, "greet.ts", "typescript").unwrap();
    for (name, doc) in [
        ("greet", "Greets a user"),
        ("ButtonProps", "Props for a button"),
    ] {
        let chunk = chunks
            .iter()
            .find(|c| c.name.as_deref() == Some(name))
            .unwrap();
        assert!(
            chunk.docstring.as_deref().is_some_and(|d| d.contains(doc)),
            "{name}: {:?}",
            chunk.docstring
        );
    }
}
