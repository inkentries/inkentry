// Integration tests verifying that LIKE metacharacters in file paths are
// escaped correctly. Each test seeds two files whose paths differ only in
// that one contains a LIKE special character (`%`, `_`, or `\`) and the
// other would match that character as a wildcard if escaping were absent;
// the storage method under test must return only the exact-match file.
//
// Tests are annotated `#[serial]` because `sqlite3_auto_extension` is
// process-global and `common::open_test_db()` must be the first call to open
// any connection in the process.

use crate::common;
use serial_test::serial;

// Insert a file + one chunk and return the file id.
fn seed_file(db: &inkentry_core::storage::Database, path: &str) -> i64 {
    let file_id = db
        .upsert_file(path, Some("rust"), "hash", 0)
        .expect("upsert_file");
    db.insert_chunk(file_id, "function", Some("f"), 1, 5, "fn f() {}", None, 3)
        .expect("insert_chunk");
    file_id
}

// A literal `%` in the query path must not match `_` or any substring in a
// bystander path. Without escaping, `%` in the LIKE pattern acts as a
// wildcard and would also match `"src/test_file.rs"`.
#[test]
#[serial]
fn chunks_for_file_percent_does_not_over_match() {
    let db = common::open_test_db();

    seed_file(&db, "src/test%file.rs");
    seed_file(&db, "src/test_file.rs");

    let results = db
        .chunks_for_file("src/test%file.rs")
        .expect("chunks_for_file");

    assert_eq!(
        results.len(),
        1,
        "expected 1 chunk for 'src/test%file.rs', got {}: {:#?}",
        results.len(),
        results
            .iter()
            .map(|r| r.file_path.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(results[0].file_path, "src/test%file.rs");
}

// A literal `_` must not match any single character. Without escaping,
// `_` in a LIKE pattern matches exactly one character, so
// `"src/testXfile.rs"` would also be returned.
#[test]
#[serial]
fn chunks_for_file_underscore_does_not_over_match() {
    let db = common::open_test_db();

    seed_file(&db, "src/test_file.rs");
    seed_file(&db, "src/testXfile.rs");

    let results = db
        .chunks_for_file("src/test_file.rs")
        .expect("chunks_for_file");

    assert_eq!(
        results.len(),
        1,
        "expected 1 chunk for 'src/test_file.rs', got {}: {:#?}",
        results.len(),
        results
            .iter()
            .map(|r| r.file_path.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(results[0].file_path, "src/test_file.rs");
}

// A literal backslash in a path must not corrupt the escape sequence and
// must return exactly one result. Paths with a backslash are unusual but
// valid (e.g. on Windows-style imports stored verbatim).
#[test]
#[serial]
fn chunks_for_file_backslash_is_handled() {
    let db = common::open_test_db();

    seed_file(&db, "src\\module\\file.rs");
    seed_file(&db, "src/module/file.rs");

    let results = db
        .chunks_for_file("src\\module\\file.rs")
        .expect("chunks_for_file");

    assert!(
        results
            .iter()
            .all(|r| r.file_path == "src\\module\\file.rs"),
        "backslash path query must not match the forward-slash path: {:#?}",
        results
            .iter()
            .map(|r| r.file_path.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        !results.is_empty(),
        "backslash path must have at least one chunk"
    );
}

// `file_paths_under` uses a prefix LIKE; a `%` in the prefix must not
// create a spurious wildcard.
#[test]
#[serial]
fn file_paths_under_percent_does_not_over_match() {
    let db = common::open_test_db();

    seed_file(&db, "src/test%dir/main.rs");
    seed_file(&db, "src/testXdir/main.rs");

    let results = db
        .file_paths_under("src/test%dir")
        .expect("file_paths_under");

    let paths: Vec<&str> = results.iter().map(|(_, p)| p.as_str()).collect();
    assert!(
        paths.contains(&"src/test%dir/main.rs"),
        "must include the literal-percent path; got: {paths:#?}"
    );
    assert!(
        !paths.contains(&"src/testXdir/main.rs"),
        "must NOT include the bystander path; got: {paths:#?}"
    );
}

// A `_` in the directory prefix must not act as a single-character wildcard.
#[test]
#[serial]
fn file_paths_under_underscore_does_not_over_match() {
    let db = common::open_test_db();

    seed_file(&db, "src/my_pkg/lib.rs");
    seed_file(&db, "src/myXpkg/lib.rs");

    let results = db.file_paths_under("src/my_pkg").expect("file_paths_under");

    let paths: Vec<&str> = results.iter().map(|(_, p)| p.as_str()).collect();
    assert!(
        paths.contains(&"src/my_pkg/lib.rs"),
        "must include the literal-underscore path; got: {paths:#?}"
    );
    assert!(
        !paths.contains(&"src/myXpkg/lib.rs"),
        "must NOT include the bystander path; got: {paths:#?}"
    );
}
