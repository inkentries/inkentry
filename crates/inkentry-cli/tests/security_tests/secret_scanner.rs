//! Regression tests for the secret-scanner bypass fix.
//!
//! Covers:
//! - a secret in a doc-comment causes the whole chunk to be dropped, so it
//!   never lands in `chunks.content`, `chunks.metadata`, or the embed queue;
//! - sensitive filenames are excluded from indexing regardless of case on a
//!   case-preserving filesystem (macOS/Windows).
//!
//! Suppression of a secret folded into a composed structural summary is covered
//! at the unit level in `inkentry_core::indexer::summariser`.

use crate::plumbing_helpers;
use plumbing_helpers::{index_project_dir, inkentry_cmd};

use predicates::prelude::*;
use tempfile::TempDir;

/// A syntactically valid AWS secret access key value (fake, for test purposes).
const FAKE_AWS_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY1";

// ── docstring secret → chunk dropped ───────────────────────────────────────────

#[test]
fn docstring_secret_drops_whole_chunk() {
    let tmp = TempDir::new().expect("create temp project dir");
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // The function body itself is clean; the secret lives only in the
    // preceding doc-comment. Before the fix, `store_chunks` only scanned
    // `chunk.content`, so this chunk was indexed, stored (docstring in
    // `metadata`), and embedded.
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

    // The chunk store must not contain the dropped chunk at all.
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

    // Directly inspect the DB: no row in `chunks` may contain the secret in
    // either `content` or `metadata` (which holds the docstring JSON), and no
    // row in `embeddings` may exist that used to hold this chunk's vector.
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

    // The chunk store must not have an embeddings row referencing the file at
    // all beyond what's expected — i.e. there is no chunk for this file, so
    // there is nothing in the embedding accumulator for it either.
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

// ── case-insensitive exclusion globs ───────────────────────────────────────────

#[test]
fn case_variant_sensitive_filenames_are_excluded() {
    let tmp = TempDir::new().expect("create temp project dir");

    // Uppercase / mixed-case variants of patterns that are already excluded in
    // lowercase form (parse_phase.rs `sensitive_patterns`).
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

/// Sanity check that the exclusion also holds for canonical lowercase names
/// (guards against a regression where `case_insensitive(true)` accidentally
/// disabled the globs entirely instead of making them case-insensitive).
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
