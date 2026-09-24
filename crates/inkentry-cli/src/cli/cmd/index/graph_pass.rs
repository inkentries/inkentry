//! Graph-edge storage for the parse phase: the per-file extraction, and the
//! re-extraction a migrated index owes for files that did not change.

use anyhow::Result;

use crate::{
    indexer::{Chunk, chunker::chunked_by_tree, graph::EdgeExtractor},
    storage::{Database, GRAPH_EDGES_REEXTRACT},
};

/// Extract and store `path_str`'s edges, replacing all of them. An unchanged
/// file re-extracted this way keeps its chunks and embeddings. Call targets
/// are resolved only for a file its chunks say was cut by its tree
/// (`chunked_by_tree`), so resolution follows the chunker's own fallback.
pub(super) fn store_edges(
    db: &Database,
    source: &str,
    path_str: &str,
    language: &str,
    chunked_by_tree: bool,
) {
    let extracted = if chunked_by_tree {
        EdgeExtractor::extract(source, path_str, language)
    } else {
        EdgeExtractor::extract_unresolved(source, path_str, language)
    };
    match extracted {
        Ok(edges) => {
            if let Err(e) = db.replace_edges(path_str, &edges) {
                tracing::warn!("graph edge storage failed for {path_str}: {e}");
            }
        }
        Err(e) => tracing::warn!("graph extraction failed for {path_str}: {e}"),
    }
}

/// `chunked_by_tree` over the chunks the parse phase will actually store, so
/// a first index decides from what a later re-extraction reads back.
pub(super) fn chunked_by_tree_as_stored(chunks: &[Chunk]) -> bool {
    chunked_by_tree(
        chunks
            .iter()
            .filter(|c| !super::parse_phase::holds_secret(c)),
    )
}

/// Whether this run owes every unchanged file a re-extraction.
pub(super) fn reextraction_owed(db: &Database) -> Result<bool> {
    db.pass_owed(GRAPH_EDGES_REEXTRACT)
}

/// Run once every file has been through the parse phase.
pub(super) fn finish(db: &Database, reextracted: bool) -> Result<()> {
    if reextracted {
        db.clear_pass_owed(GRAPH_EDGES_REEXTRACT)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::IndexArgs;
    use super::super::parse_phase::{missing_embedding_texts, run_parse_phase};
    use crate::storage::{Database, GRAPH_EDGES_REEXTRACT};
    use indicatif::MultiProgress;
    use std::path::Path;
    use std::sync::OnceLock;

    fn open_db() -> Database {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
        Database::open(Path::new(":memory:")).expect("open in-memory Database")
    }

    fn index(root: &Path, db: &Database) {
        let args = IndexArgs {
            path: root.to_path_buf(),
            db: None,
            batch_size: 64,
            force: false,
            recount: false,
            no_summaries: false,
            background_phases: false,
            embed_phases: false,
            detach: false,
            detach_embed: false,
            config_path: None,
        };
        let cfg = crate::config::Config::default();
        run_parse_phase(root, db, &args, &MultiProgress::new(), &cfg).expect("parse phase");
    }

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir
    }

    fn target_of(db: &Database, file: &str, source: &str) -> Vec<(String, Option<String>)> {
        db.edges_for_file(file)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "calls" && e.source_name.as_deref() == Some(source))
            .map(|e| (e.target_name, e.target_file))
            .collect()
    }

    fn resolved(file: &str, target: &str) -> Vec<(String, Option<String>)> {
        vec![(target.to_owned(), Some(file.to_owned()))]
    }

    #[test]
    fn a_callee_defined_in_the_callers_file_and_elsewhere_resolves_to_the_callers_file() {
        let dir = tree(&[
            ("src/a.rs", "fn helper() {}\npub fn go() { helper(); }\n"),
            ("src/b.rs", "pub fn helper() {}\n"),
        ]);
        let db = open_db();
        index(dir.path(), &db);
        assert_eq!(
            target_of(&db, "src/a.rs", "go"),
            resolved("src/a.rs", "helper")
        );
    }

    #[test]
    fn a_callee_defined_only_in_another_file_stays_unresolved() {
        let dir = tree(&[
            ("src/a.rs", "pub fn go() { helper(); }\n"),
            ("src/b.rs", "pub fn helper() {}\n"),
        ]);
        let db = open_db();
        index(dir.path(), &db);
        assert_eq!(
            target_of(&db, "src/a.rs", "go"),
            vec![("helper".to_owned(), None)]
        );
    }

    #[test]
    fn a_receiver_call_to_a_name_defined_in_exactly_one_file_stays_unresolved() {
        let dir = tree(&[
            ("src/a.py", "def go(items):\n    return items.find(1)\n"),
            ("src/b.py", "def find(x):\n    return x\n"),
        ]);
        let db = open_db();
        index(dir.path(), &db);
        assert_eq!(
            target_of(&db, "src/a.py", "go"),
            vec![("find".to_owned(), None)]
        );
    }

    #[test]
    fn a_callee_bound_to_a_file_level_const_is_never_claimed_by_another_file() {
        let dir = tree(&[
            (
                "src/a.js",
                "const helper = () => 1;\nexport function go() { return helper(); }\n",
            ),
            ("src/b.js", "export function helper() { return 2; }\n"),
        ]);
        let db = open_db();
        index(dir.path(), &db);
        assert_eq!(
            target_of(&db, "src/a.js", "go"),
            resolved("src/a.js", "helper")
        );
    }

    #[test]
    fn a_callee_bound_to_a_module_level_assignment_is_never_claimed_by_another_file() {
        let dir = tree(&[
            (
                "app/a.py",
                "helper = make()\n\ndef go():\n    return helper()\n",
            ),
            ("app/b.py", "def helper():\n    return 2\n"),
        ]);
        let db = open_db();
        index(dir.path(), &db);
        // `make()` returns whatever it returns: this file binds the name but
        // defines nothing a call could be said to reach.
        assert_eq!(
            target_of(&db, "app/a.py", "go"),
            vec![("helper".to_owned(), None)]
        );
    }

    #[test]
    fn a_call_through_an_import_alias_targets_the_imported_name_without_placing_it() {
        let dir = tree(&[
            (
                "src/a.ts",
                "import { foo as bar } from './lib';\nexport function run() { return bar(); }\n",
            ),
            ("src/lib.ts", "export function foo() { return 1; }\n"),
        ]);
        let db = open_db();
        index(dir.path(), &db);
        // Which file holds the imported definition is a cross-file question.
        assert_eq!(
            target_of(&db, "src/a.ts", "run"),
            vec![("foo".to_owned(), None)]
        );
    }

    #[test]
    fn the_first_index_reads_the_tree_outcome_off_the_chunks_it_will_store() {
        use inkentry_core::indexer::{Chunk, ChunkKind};
        let chunk = |kind, name: Option<&str>, content: &str| Chunk {
            file_path: "a.py".into(),
            language: "python".into(),
            kind,
            name: name.map(str::to_owned),
            start_line: 1,
            end_line: 2,
            content: content.into(),
            docstring: None,
            parent_scope: None,
            summary: None,
        };
        // The only chunk that came from the tree is dropped as a secret, so
        // what gets stored reads as a window, and so must this decision.
        let chunks = [
            chunk(
                ChunkKind::Function,
                Some("load"),
                "key = AKIAIOSFODNN7EXAMPLE",
            ),
            chunk(ChunkKind::Verbatim, None, "x = 1"),
        ];
        assert!(!super::chunked_by_tree_as_stored(&chunks));
        let clean = [chunk(ChunkKind::Function, Some("load"), "return 1")];
        assert!(super::chunked_by_tree_as_stored(&clean));
    }

    fn file_level_calls(db: &Database, file: &str) -> Vec<(String, Option<String>)> {
        db.edges_for_file(file)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "calls" && e.source_name.is_none())
            .map(|e| (e.target_name, e.target_file))
            .collect()
    }

    #[test]
    fn a_file_the_chunker_windows_gets_no_locals_pass_on_either_path() {
        // No function or class, so the chunker falls back to a sliding
        // window; the file-level lambda would otherwise resolve `helper`.
        let windowed = "helper = lambda: 1\nhelper()\n";
        let treed = "helper = lambda: 1\nhelper()\n\ndef run():\n    return 1\n";
        let dir = tree(&[("app/w.py", windowed), ("app/t.py", treed)]);
        let db = open_db();
        index(dir.path(), &db);
        let unresolved = vec![("helper".to_owned(), None)];
        assert_eq!(file_level_calls(&db, "app/w.py"), unresolved);
        assert_eq!(
            file_level_calls(&db, "app/t.py"),
            vec![("helper".to_owned(), Some("app/t.py".to_owned()))]
        );

        db.mark_pass_owed(GRAPH_EDGES_REEXTRACT).unwrap();
        index(dir.path(), &db);
        assert_eq!(
            file_level_calls(&db, "app/w.py"),
            unresolved,
            "re-extracting an unchanged file follows the chunks it has"
        );
    }

    fn all_edges(db: &Database) -> Vec<String> {
        let mut rows: Vec<String> = ["src/a.rs", "src/b.rs", "src/c.py"]
            .iter()
            .flat_map(|f| db.edges_for_file(f).unwrap())
            .map(|e| serde_json::to_string(&e).unwrap())
            .collect();
        rows.sort();
        rows
    }

    const MIXED_TREE: &[(&str, &str)] = &[
        (
            "src/a.rs",
            "fn helper() {}\npub fn go(parse: fn()) { helper(); parse(); shared(); }\n",
        ),
        ("src/b.rs", "pub fn shared() {}\npub fn parse() {}\n"),
        ("src/c.py", "def run():\n    return shared()\n"),
    ];

    #[test]
    fn two_indexes_of_the_same_tree_resolve_identically() {
        let dir = tree(MIXED_TREE);
        let (first, second) = (open_db(), open_db());
        index(dir.path(), &first);
        index(dir.path(), &second);
        let edges = all_edges(&first);
        assert!(
            edges.iter().any(|e| e.contains("target_file")),
            "the tree must resolve something for this to compare anything: {edges:?}"
        );
        assert_eq!(edges, all_edges(&second));
    }

    #[test]
    fn an_owed_re_extraction_fills_target_file_for_unchanged_files_without_re_embedding() {
        let dir = tree(MIXED_TREE);
        let db = open_db();
        index(dir.path(), &db);
        let expected = all_edges(&db);
        let chunk_ids: Vec<i64> = missing_embedding_texts(&db)
            .unwrap()
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        for &id in &chunk_ids {
            db.insert_embedding(id, &[0.1f32; 896]).unwrap();
        }

        // What a store migrated from before `target_file` existed holds:
        // every edge unresolved, the call to the `parse` parameter still
        // there, and the re-extraction owed.
        for file in ["src/a.rs", "src/b.rs", "src/c.py"] {
            let mut unresolved: Vec<_> = db
                .edges_for_file(file)
                .unwrap()
                .into_iter()
                .map(|e| inkentry_core::indexer::graph::Edge {
                    source_file: e.source_file,
                    source_name: e.source_name,
                    target_name: e.target_name,
                    kind: inkentry_core::indexer::graph::EdgeKind::parse(&e.kind),
                    line: e.line,
                    target_file: None,
                })
                .collect();
            if file == "src/a.rs" {
                let mut shadowed = unresolved[0].clone();
                shadowed.target_name = "parse".to_owned();
                shadowed.source_name = Some("go".to_owned());
                shadowed.kind = inkentry_core::indexer::graph::EdgeKind::Calls;
                unresolved.push(shadowed);
            }
            db.replace_edges(file, &unresolved).unwrap();
        }
        db.mark_pass_owed(GRAPH_EDGES_REEXTRACT).unwrap();

        index(dir.path(), &db);

        assert_eq!(all_edges(&db), expected);
        assert!(!db.pass_owed(GRAPH_EDGES_REEXTRACT).unwrap());
        assert!(
            missing_embedding_texts(&db).unwrap().is_empty(),
            "no chunk may lose its vector or be queued to re-embed"
        );
        assert_eq!(
            db.stats().unwrap().embedding_count as usize,
            chunk_ids.len()
        );
    }
}
