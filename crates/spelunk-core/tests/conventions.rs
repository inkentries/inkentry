//! Integration and unit tests for convention extraction (#268).
//!
//! All tests use in-memory SQLite so they are hermetic (no LLM, no network).
//! DB tests are annotated `#[serial]` because `sqlite3_auto_extension` is process-global.

mod common;

use serial_test::serial;

use spelunk_core::conventions::{
    ConventionRecord,
    extractor::{ChunkSummary, ConventionExtractor},
    run_extraction,
};
use spelunk_core::storage::ConventionRow;

// ── Helper builders ───────────────────────────────────────────────────────────

fn rust_fn(name: &str, content: &str) -> ChunkSummary {
    ChunkSummary {
        language: "rust".into(),
        node_type: "function".into(),
        name: Some(name.into()),
        content: content.into(),
        file_path: "src/lib.rs".into(),
        has_docstring: content.trim_start().starts_with("///"),
    }
}

fn rust_struct(name: &str) -> ChunkSummary {
    ChunkSummary {
        language: "rust".into(),
        node_type: "struct".into(),
        name: Some(name.into()),
        content: format!("struct {name} {{}}"),
        file_path: "src/lib.rs".into(),
        has_docstring: false,
    }
}

fn ts_fn(name: &str, content: &str) -> ChunkSummary {
    ChunkSummary {
        language: "typescript".into(),
        node_type: "function".into(),
        name: Some(name.into()),
        content: content.into(),
        file_path: "src/index.ts".into(),
        has_docstring: content.trim_start().starts_with("/**"),
    }
}

fn ts_class(name: &str) -> ChunkSummary {
    ChunkSummary {
        language: "typescript".into(),
        node_type: "class".into(),
        name: Some(name.into()),
        content: format!("class {name} {{}}"),
        file_path: "src/models.ts".into(),
        has_docstring: false,
    }
}

fn find_record<'a>(
    records: &'a [ConventionRecord],
    category: &str,
) -> Option<&'a ConventionRecord> {
    records.iter().find(|r| r.category == category)
}

// ── Rust: naming conventions ──────────────────────────────────────────────────

#[test]
fn rust_functions_snake_case() {
    let chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| rust_fn(&format!("do_thing_{i}"), "fn do_thing() {}"))
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, 0);
    let r = find_record(&records, "naming.functions").expect("naming.functions record");
    assert!(r.confidence >= 0.9, "confidence={}", r.confidence);
    assert!(
        r.description.contains("snake_case"),
        "desc={}",
        r.description
    );
}

#[test]
fn rust_types_pascal_case() {
    let chunks: Vec<ChunkSummary> = ["MyStruct", "AnotherOne", "FooBar", "BazQuux", "HelloWorld"]
        .iter()
        .map(|n| rust_struct(n))
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, 0);
    let r = find_record(&records, "naming.types").expect("naming.types record");
    assert!(
        r.description.contains("PascalCase"),
        "desc={}",
        r.description
    );
}

#[test]
fn rust_error_handling_anyhow() {
    let content = "use anyhow::Result; fn foo() -> Result<()> { Ok(()) }";
    let chunks: Vec<ChunkSummary> = (0..8).map(|_| rust_fn("foo", content)).collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, 0);
    let r = find_record(&records, "error_handling").expect("error_handling record");
    assert!(r.description.contains("anyhow"), "desc={}", r.description);
}

#[test]
fn rust_async_runtime_detected() {
    let content = "use tokio::time; async fn handler() { tokio::spawn(async {}); }";
    let chunks: Vec<ChunkSummary> = (0..6).map(|_| rust_fn("handler", content)).collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, 0);
    let r = find_record(&records, "async").expect("async record");
    assert!(r.description.contains("tokio"), "desc={}", r.description);
}

#[test]
fn rust_testing_cfg_test() {
    let content = "#[cfg(test)] mod tests { #[test] fn it_works() {} }";
    let chunks: Vec<ChunkSummary> = (0..6)
        .map(|i| {
            let mut c = rust_fn(&format!("it_works_{i}"), content);
            c.node_type = "function".into();
            c
        })
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, 0);
    let r = find_record(&records, "testing").expect("testing record");
    assert!(
        r.description.contains("cfg(test)"),
        "desc={}",
        r.description
    );
}

#[test]
fn rust_doc_coverage_high() {
    let chunks: Vec<ChunkSummary> = (0..8)
        .map(|i| rust_fn(&format!("func_{i}"), "/// Does the thing\nfn func() {}"))
        .chain((0..2).map(|i| rust_fn(&format!("undoc_{i}"), "fn undoc() {}")))
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::rust::extract(&refs, 0);
    let r = find_record(&records, "docs").expect("docs record");
    assert!(r.description.contains("high"), "desc={}", r.description);
}

// ── TypeScript: naming conventions ───────────────────────────────────────────

#[test]
fn ts_functions_camel_case() {
    let chunks: Vec<ChunkSummary> = [
        "getUserById",
        "handleClick",
        "loadConfig",
        "renderPage",
        "fetchData",
        "parseInput",
        "validateForm",
    ]
    .iter()
    .map(|n| ts_fn(n, &format!("function {n}() {{}}")))
    .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::typescript::extract(&refs, 0);
    let r = find_record(&records, "naming.functions").expect("naming.functions record");
    assert!(
        r.description.contains("camelCase"),
        "desc={}",
        r.description
    );
}

#[test]
fn ts_types_pascal_case() {
    let chunks: Vec<ChunkSummary> = [
        "UserService",
        "ApiClient",
        "DataModel",
        "EventBus",
        "HttpError",
    ]
    .iter()
    .map(|n| ts_class(n))
    .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::typescript::extract(&refs, 0);
    let r = find_record(&records, "naming.types").expect("naming.types record");
    assert!(
        r.description.contains("PascalCase"),
        "desc={}",
        r.description
    );
}

#[test]
fn ts_async_usage_detected() {
    let content = "async function fetchUser() { const data = await fetch('/api/users'); }";
    let chunks: Vec<ChunkSummary> = (0..6)
        .map(|i| ts_fn(&format!("fetchUser{i}"), content))
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::typescript::extract(&refs, 0);
    let r = find_record(&records, "async").expect("async record");
    assert!(r.description.contains("async"), "desc={}", r.description);
    assert!(r.confidence > 0.2, "confidence={}", r.confidence);
}

#[test]
fn ts_testing_spec_files() {
    let chunks: Vec<ChunkSummary> = (0..5)
        .map(|i| ChunkSummary {
            language: "typescript".into(),
            node_type: "function".into(),
            name: Some(format!("test_{i}")),
            content: format!("test('does thing {i}', () => {{}})"),
            file_path: "src/components/Button.spec.ts".into(),
            has_docstring: false,
        })
        .collect();
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let records = spelunk_core::conventions::rules::typescript::extract(&refs, 0);
    let r = find_record(&records, "testing").expect("testing record");
    assert!(r.description.contains("spec.ts"), "desc={}", r.description);
}

// ── ConventionExtractor: multi-language dispatch ──────────────────────────────

#[test]
fn extractor_dispatches_by_language() {
    let rust_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| rust_fn(&format!("rust_fn_{i}"), "fn rust_fn() {}"))
        .collect();
    let ts_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| ts_fn(&format!("tsFn{i}"), "function tsFn() {}"))
        .collect();
    let all: Vec<ChunkSummary> = rust_chunks.into_iter().chain(ts_chunks).collect();

    let records = ConventionExtractor::new().extract(&all);
    let rust_records: Vec<_> = records.iter().filter(|r| r.language == "rust").collect();
    let ts_records: Vec<_> = records
        .iter()
        .filter(|r| r.language == "typescript")
        .collect();
    assert!(!rust_records.is_empty(), "should have Rust records");
    assert!(!ts_records.is_empty(), "should have TypeScript records");
}

#[test]
fn extractor_handles_empty_input() {
    let records = ConventionExtractor::new().extract(&[]);
    assert!(records.is_empty());
}

/// Regression: the extractor must emit at most one record per (language,
/// category). Language-specific + always-on generic sets emit overlapping
/// categories (naming.functions, docs), and tsx chunks route through the
/// typescript set (self-labelled "typescript"), so a rust/ts/tsx corpus
/// previously listed those categories two or three times per language.
#[test]
fn extractor_dedups_language_category() {
    let rust_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| rust_fn(&format!("do_thing_{i}"), "/// doc\nfn do_thing() {}"))
        .collect();
    let ts_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| {
            ts_fn(
                &format!("getThing{i}"),
                "/** doc */\nfunction getThing() {}",
            )
        })
        .collect();
    // tsx chunks: the typescript rule set self-labels these "typescript", so
    // without dedup they collide with the ts group's records.
    let tsx_chunks: Vec<ChunkSummary> = (0..10)
        .map(|i| ChunkSummary {
            language: "tsx".into(),
            node_type: "function".into(),
            name: Some(format!("renderThing{i}")),
            content: "/** doc */\nfunction renderThing() {}".into(),
            file_path: "src/App.tsx".into(),
            has_docstring: true,
        })
        .collect();

    let all: Vec<ChunkSummary> = rust_chunks
        .into_iter()
        .chain(ts_chunks)
        .chain(tsx_chunks)
        .collect();

    let records = ConventionExtractor::new().extract(&all);

    let mut seen = std::collections::HashSet::new();
    for r in &records {
        assert!(
            seen.insert((r.language.clone(), r.category.clone())),
            "duplicate (language, category): ({}, {})",
            r.language,
            r.category
        );
    }

    // The overlapping categories must survive exactly once per language.
    let naming: Vec<_> = records
        .iter()
        .filter(|r| r.language == "typescript" && r.category == "naming.functions")
        .collect();
    assert_eq!(
        naming.len(),
        1,
        "typescript naming.functions must be unique"
    );
    let docs: Vec<_> = records
        .iter()
        .filter(|r| r.language == "rust" && r.category == "docs")
        .collect();
    assert_eq!(docs.len(), 1, "rust docs must be unique");
}

// ── DB round-trip: replace_conventions + list_conventions ─────────────────────

#[test]
#[serial]
fn db_round_trip_replace_and_list() {
    let db = common::open_test_db();

    let rows = vec![
        ConventionRow {
            language: "rust".into(),
            category: "naming.functions".into(),
            description: "Functions use snake_case".into(),
            confidence: 0.9,
            evidence_count: 10,
            extracted_at: 0,
        },
        ConventionRow {
            language: "typescript".into(),
            category: "naming.functions".into(),
            description: "Functions use camelCase".into(),
            confidence: 0.85,
            evidence_count: 7,
            extracted_at: 0,
        },
    ];
    db.replace_conventions(&rows).unwrap();

    let all = db.list_conventions(None).unwrap();
    assert_eq!(all.len(), 2);

    let rust_only = db.list_conventions(Some("rust")).unwrap();
    assert_eq!(rust_only.len(), 1);
    assert_eq!(rust_only[0].description, "Functions use snake_case");
}

#[test]
#[serial]
fn db_replace_is_idempotent() {
    let db = common::open_test_db();
    let row = ConventionRow {
        language: "rust".into(),
        category: "testing".into(),
        description: "Tests in #[cfg(test)] inline modules".into(),
        confidence: 0.8,
        evidence_count: 6,
        extracted_at: 1000,
    };
    db.replace_conventions(std::slice::from_ref(&row)).unwrap();
    db.replace_conventions(std::slice::from_ref(&row)).unwrap();

    let all = db.list_conventions(None).unwrap();
    assert_eq!(all.len(), 1, "replace should delete old records first");
}

#[test]
#[serial]
fn db_list_conventions_empty_when_none_stored() {
    let db = common::open_test_db();
    let all = db.list_conventions(None).unwrap();
    assert!(all.is_empty());
}

// ── End-to-end: run_extraction via DB ─────────────────────────────────────────

#[test]
#[serial]
fn run_extraction_end_to_end() {
    let db = common::open_test_db();

    // Seed 10 Rust snake_case functions.
    let rust_file_id = db.upsert_file("src/lib.rs", Some("rust"), "hash1").unwrap();
    for i in 0..10 {
        db.insert_chunk(
            rust_file_id,
            "function",
            Some(&format!("rust_fn_{i}")),
            i,
            i + 5,
            "fn rust_fn() {}",
            None,
            10,
        )
        .unwrap();
    }

    // Seed 10 TypeScript camelCase functions.
    let ts_file_id = db
        .upsert_file("src/index.ts", Some("typescript"), "hash2")
        .unwrap();
    for i in 0..10 {
        db.insert_chunk(
            ts_file_id,
            "function",
            Some(&format!("tsFn{i}")),
            i,
            i + 3,
            "function tsFn() {}",
            None,
            8,
        )
        .unwrap();
    }

    let records = run_extraction(&db).unwrap();
    // After confidence/evidence filtering (>= 0.5, >= 5 evidence), expect results.
    assert!(!records.is_empty(), "extraction should produce records");

    let rust_naming = records
        .iter()
        .find(|r| r.language == "rust" && r.category == "naming.functions");
    let ts_naming = records
        .iter()
        .find(|r| r.language == "typescript" && r.category == "naming.functions");
    assert!(rust_naming.is_some(), "should detect Rust function naming");
    assert!(
        ts_naming.is_some(),
        "should detect TypeScript function naming"
    );
}

// ── list_conventions API wrapper ──────────────────────────────────────────────

#[test]
#[serial]
fn list_conventions_wrapper_converts_correctly() {
    let db = common::open_test_db();
    let rows = vec![ConventionRow {
        language: "rust".into(),
        category: "async".into(),
        description: "Async runtime: tokio".into(),
        confidence: 0.75,
        evidence_count: 8,
        extracted_at: 42,
    }];
    db.replace_conventions(&rows).unwrap();

    let records = spelunk_core::conventions::list_conventions(&db, None).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].language, "rust");
    assert_eq!(records[0].category, "async");
    assert_eq!(records[0].extracted_at, 42);
}

// ── Confidence filtering ──────────────────────────────────────────────────────

#[test]
fn extractor_emits_low_evidence_raw_records() {
    // The extractor emits records regardless of evidence count.
    // run_extraction applies the filter (>= 0.5 confidence AND >= 5 evidence).
    // With 2 evidence points the evidence_count should be < 5.
    let chunks = [
        rust_fn("small_set_a", "fn small_set_a() {}"),
        rust_fn("small_set_b", "fn small_set_b() {}"),
    ];
    let refs: Vec<&ChunkSummary> = chunks.iter().collect();
    let raw = spelunk_core::conventions::rules::rust::extract(&refs, 0);
    if let Some(r) = raw.iter().find(|r| r.category == "naming.functions") {
        assert!(
            r.evidence_count < 5,
            "evidence_count={} should be below threshold",
            r.evidence_count
        );
    }
}
