// Tests for the golden-schema checker itself.
//
// Without these, the per-command conformance tests could pass vacuously: a
// checker that never rejects anything makes every golden file green. These pin
// the two halves of the additive-only rule directly on the checker, using
// synthetic rows rather than real command output.

mod schema_contract;
use schema_contract::{CommandSchema, Violation, check_rows, parse_golden};

fn schema(json: &str) -> CommandSchema {
    let doc = format!(r#"{{"commands": {{"probe": {json}}}}}"#);
    parse_golden(&doc).remove("probe").expect("probe schema")
}

fn row(json: &str) -> Vec<serde_json::Value> {
    vec![serde_json::from_str(json).expect("test row is valid JSON")]
}

fn simple_schema() -> CommandSchema {
    schema(r#"{"required": {"path": "string", "count": "integer"}}"#)
}

#[test]
fn conforming_row_yields_no_violations() {
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": 3}"#),
    );
    assert_eq!(violations, vec![]);
}

#[test]
fn adding_an_undeclared_field_is_accepted() {
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": 3, "brand_new_field": "hello"}"#),
    );
    assert_eq!(
        violations,
        vec![],
        "additive evolution is explicitly permitted by the contract"
    );
}

#[test]
fn removing_a_required_field_is_rejected() {
    let violations = check_rows(&simple_schema(), &row(r#"{"path": "src/lib.rs"}"#));
    assert_eq!(
        violations,
        vec![Violation::MissingField {
            line: 1,
            field: "count".to_string(),
        }]
    );
}

#[test]
fn renaming_a_required_field_is_rejected() {
    // A rename is indistinguishable from a removal plus an addition, and the
    // removal half is what breaks every existing consumer.
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "chunk_count": 3}"#),
    );
    assert_eq!(
        violations,
        vec![Violation::MissingField {
            line: 1,
            field: "count".to_string(),
        }]
    );
}

#[test]
fn retyping_a_required_field_is_rejected() {
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": "3"}"#),
    );
    assert_eq!(
        violations,
        vec![Violation::WrongType {
            line: 1,
            field: "count".to_string(),
            expected: "integer".to_string(),
            actual: "string".to_string(),
        }]
    );
}

#[test]
fn widening_an_integer_field_to_a_float_is_rejected() {
    let violations = check_rows(
        &simple_schema(),
        &row(r#"{"path": "src/lib.rs", "count": 3.5}"#),
    );
    assert_eq!(
        violations,
        vec![Violation::WrongType {
            line: 1,
            field: "count".to_string(),
            expected: "integer".to_string(),
            actual: "number".to_string(),
        }]
    );
}

#[test]
fn a_nullable_field_accepts_both_null_and_its_type() {
    let nullable = schema(r#"{"required": {"name": "string|null"}}"#);
    assert_eq!(check_rows(&nullable, &row(r#"{"name": "parse"}"#)), vec![]);
    assert_eq!(check_rows(&nullable, &row(r#"{"name": null}"#)), vec![]);
    assert_eq!(
        check_rows(&nullable, &row(r#"{"name": 7}"#)),
        vec![Violation::WrongType {
            line: 1,
            field: "name".to_string(),
            expected: "string|null".to_string(),
            actual: "integer".to_string(),
        }]
    );
}

#[test]
fn an_array_field_checks_its_element_type() {
    let arrays = schema(r#"{"required": {"tags": "array<string>"}}"#);
    assert_eq!(check_rows(&arrays, &row(r#"{"tags": []}"#)), vec![]);
    assert_eq!(check_rows(&arrays, &row(r#"{"tags": ["a", "b"]}"#)), vec![]);
    assert_eq!(
        check_rows(&arrays, &row(r#"{"tags": [1, 2]}"#)),
        vec![Violation::WrongType {
            line: 1,
            field: "tags".to_string(),
            expected: "array<string>".to_string(),
            actual: "array<integer>".to_string(),
        }]
    );
}

#[test]
fn an_omitted_optional_field_is_accepted_but_a_mistyped_one_is_not() {
    let with_optional =
        schema(r#"{"required": {"id": "integer"}, "optional": {"source_ref": "string"}}"#);
    assert_eq!(check_rows(&with_optional, &row(r#"{"id": 1}"#)), vec![]);
    assert_eq!(
        check_rows(&with_optional, &row(r#"{"id": 1, "source_ref": "abc123"}"#)),
        vec![]
    );
    assert_eq!(
        check_rows(&with_optional, &row(r#"{"id": 1, "source_ref": 42}"#)),
        vec![Violation::WrongType {
            line: 1,
            field: "source_ref".to_string(),
            expected: "string".to_string(),
            actual: "integer".to_string(),
        }]
    );
}

#[test]
fn every_emitted_line_is_checked_not_just_the_first() {
    let rows = vec![
        serde_json::json!({"path": "a.rs", "count": 1}),
        serde_json::json!({"path": "b.rs"}),
    ];
    assert_eq!(
        check_rows(&simple_schema(), &rows),
        vec![Violation::MissingField {
            line: 2,
            field: "count".to_string(),
        }]
    );
}

#[test]
fn a_non_object_line_is_rejected() {
    let violations = check_rows(&simple_schema(), &row(r#"["not", "an", "object"]"#));
    assert_eq!(violations, vec![Violation::NotAnObject { line: 1 }]);
}
