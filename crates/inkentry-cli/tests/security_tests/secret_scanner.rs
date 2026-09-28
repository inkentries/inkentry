// A secret in a doc-comment must drop the whole chunk, so it never lands in
// `chunks.content`, `chunks.metadata`, or the embed queue. Sensitive filenames
// are excluded from indexing regardless of case on a case-preserving
// filesystem (macOS/Windows).
//
// A secret folded into a composed structural summary is covered at the unit
// level in `inkentry_core::indexer::summariser`.

use crate::plumbing_helpers;
use plumbing_helpers::{index_project_dir, inkentry_cmd};

use predicates::prelude::*;
use tempfile::TempDir;

// A syntactically valid but fake AWS secret access key.
const FAKE_AWS_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY1";

#[test]
fn docstring_secret_drops_whole_chunk() {
    let tmp = TempDir::new().expect("create temp project dir");
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // The function body is clean; the secret lives only in the preceding
    // doc-comment.
    let source = format!(
        "/// aws_secret_access_key = \"{FAKE_AWS_SECRET}\"\npub fn clean_fn(x: i32) -> i32 {{\n    x + 1\n}}\n"
    );
    std::fs::write(src_dir.join("lib.rs"), &source).unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"secret-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let (_tmp_idx, db_path, config_path) = index_project_dir(tmp.path());

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("cat-chunks")
        .arg("src/lib.rs")
        .assert()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(
        !text.contains("clean_fn"),
        "chunk with a secret in its docstring must be dropped entirely, got: {text}"
    );
    assert!(
        !text.contains(FAKE_AWS_SECRET),
        "the secret must never appear in cat-chunks output"
    );

    // No row in `chunks` may hold the secret in `content` or `metadata`
    // (docstring JSON).
    let conn = rusqlite::Connection::open(&db_path).expect("open db");
    let mut stmt = conn
        .prepare("SELECT content, metadata FROM chunks")
        .unwrap();
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for (content, metadata) in &rows {
        assert!(
            !content.contains(FAKE_AWS_SECRET),
            "secret leaked into chunks.content: {content}"
        );
        if let Some(m) = metadata {
            assert!(
                !m.contains(FAKE_AWS_SECRET),
                "secret leaked into chunks.metadata (docstring): {m}"
            );
        }
    }

    // No chunk row exists for this file, so nothing exists for it in
    // embeddings either.
    let chunk_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks c JOIN files f ON c.file_id = f.id WHERE f.path LIKE '%lib.rs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        chunk_count, 0,
        "the only chunk in this file contained a secret and must have been dropped"
    );
}

#[test]
fn case_variant_sensitive_filenames_are_excluded() {
    let tmp = TempDir::new().expect("create temp project dir");

    // Case variants of patterns already excluded in lowercase form
    // (parse_phase.rs `sensitive_patterns`).
    std::fs::write(tmp.path().join("ID_RSA"), "fake private key material\n").unwrap();
    std::fs::write(tmp.path().join(".ENV"), "SECRET=fake\n").unwrap();
    std::fs::write(
        tmp.path().join("Config.PEM"),
        "-----BEGIN CERTIFICATE-----\n",
    )
    .unwrap();
    // Control: an ordinary source file that must still be indexed.
    std::fs::write(
        tmp.path().join("main.rs"),
        "pub fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"case-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let (_tmp_idx, db_path, config_path) = index_project_dir(tmp.path());

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("ls-files")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();

    for excluded in ["ID_RSA", ".ENV", "Config.PEM"] {
        assert!(
            !text.to_lowercase().contains(&excluded.to_lowercase()),
            "expected '{excluded}' to be excluded from indexing regardless of case, \
             ls-files output: {text}"
        );
    }
    assert!(
        text.contains("main.rs"),
        "expected the ordinary source file to still be indexed, ls-files output: {text}"
    );
}

// Guards against `case_insensitive(true)` disabling the globs entirely
// instead of making them case-insensitive.
#[test]
fn lowercase_sensitive_filenames_still_excluded() {
    let tmp = TempDir::new().expect("create temp project dir");
    std::fs::write(tmp.path().join("id_rsa"), "fake private key material\n").unwrap();
    std::fs::write(
        tmp.path().join("main.rs"),
        "pub fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"case-fixture-2\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();

    let (_tmp_idx, db_path, config_path) = index_project_dir(tmp.path());

    inkentry_cmd(&db_path, &config_path)
        .arg("ls-files")
        .assert()
        .success()
        .stdout(predicate::str::contains("id_rsa").not());
}
