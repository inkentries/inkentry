// Upgrade corpus ("DB museum"): open artifacts written by real released
// binaries with the current build and assert nothing is lost.
//
// Every wing under `fixtures/upgrade-corpus/wings/` was produced by an actual
// downloaded release, not by constructing an old shape by hand. The expected
// values in MANIFEST.json were read out of each artifact at capture time with
// plain SQL, before any current-binary code touched it, so they are an
// independent record of what the old binary wrote rather than an echo of what
// today's code happens to produce.
//
// The corpus holds three wings, and the reason it holds so few is the point.
//
// A wing earns its place by covering a path a real user's data actually takes.
// No database written by the earlier product is such a path: its `index.db` is
// not carried at all (the user reindexes) and its `memory.db` is exported to a
// portable dump and imported into a store this binary creates. Wings for those
// were archaeology, and they went with the migration ladders they were
// defending.
//
// Both databases written by 1.0 or 1.1 are such a path. `memory.db` is stamped
// schema version 11 and `index.db` 17, and each is migrated forward in place,
// so the store a real 1.1.0 binary wrote is what the first step after it has
// to carry across.
//
// The notes ref is the exception, and it is why the harness survives them. It
// is renamed in place rather than exported, so a migrating user really does
// hand this binary a ref carrying blobs from three older writing eras.
//
// The harness is kept whole for the wings that do not exist yet. The first time
// a shipped schema version has to migrate to a newer one, that release's
// databases get captured here and this file grows a test that opens them; the
// tripwire at the bottom is what makes that happen rather than be forgotten.
//
// Regenerate with scripts/upgrade-corpus/generate.sh.

use std::path::{Path, PathBuf};

use inkentry_core::storage::{
    Database, GRAPH_EDGES_REEXTRACT, GitNotesBackend, MemoryBackend, MemoryStore,
};
use inkentry_core::test_support::git_command;
use serde::Deserialize;

// sqlite-vec is registered process-globally, before any connection is opened.
// Without it every vec0 table fails to load and a row-count assertion would be
// reading an error, not an empty table.
fn register_sqlite_vec() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

#[derive(Debug, Deserialize)]
struct Manifest {
    wings: Vec<Wing>,
}

#[derive(Debug, Deserialize)]
struct Wing {
    id: String,
    producer: String,
    kind: String,
    artifact: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    expect: Expect,
}

#[derive(Debug, Default, Deserialize)]
struct Expect {
    #[serde(default)]
    era_entries: Vec<EraEntry>,
    #[serde(default)]
    schema_version: i32,
    #[serde(default)]
    note_count: usize,
    #[serde(default)]
    active_note_count: usize,
    #[serde(default)]
    archived_title: String,
    #[serde(default)]
    superseded_title: String,
    #[serde(default)]
    successor_title: String,
    #[serde(default)]
    raw_tags_and_files: Vec<RawTagsAndFiles>,
    #[serde(default)]
    file_count: usize,
    #[serde(default)]
    chunk_count: usize,
    #[serde(default)]
    embedding_count: usize,
    #[serde(default)]
    graph_edge_count: usize,
}

#[derive(Debug, Deserialize)]
struct RawTagsAndFiles {
    title: String,
    tags: String,
    linked_files: String,
}

#[derive(Debug, Deserialize)]
struct EraEntry {
    title: String,
    kind: String,
    body: String,
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("upgrade-corpus")
}

fn manifest() -> Manifest {
    let path = corpus_root().join("MANIFEST.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading corpus manifest {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("parsing corpus manifest")
}

// Expand a wing's artifact into a temp dir. Every test works on that copy:
// opening a database migrates it, which would otherwise rewrite the checked-in
// fixture and destroy the very thing under test on the first run.
//
// Database artifacts are stored gzipped because a captured database is mostly
// the vec0 extension's preallocated vector chunk, and that is zeros.
fn checkout(wing: &Wing, tmp: &Path) -> PathBuf {
    let src = corpus_root()
        .join("wings")
        .join(&wing.id)
        .join(&wing.artifact);
    let dst = tmp.join(wing.artifact.trim_end_matches(".gz"));

    if !wing.artifact.ends_with(".gz") {
        std::fs::copy(&src, &dst)
            .unwrap_or_else(|e| panic!("copying wing {} from {}: {e}", wing.id, src.display()));
        return dst;
    }

    let packed = std::fs::File::open(&src)
        .unwrap_or_else(|e| panic!("opening wing {} at {}: {e}", wing.id, src.display()));
    let mut reader = flate2::read::GzDecoder::new(std::io::BufReader::new(packed));
    let mut out =
        std::fs::File::create(&dst).unwrap_or_else(|e| panic!("creating {}: {e}", dst.display()));
    std::io::copy(&mut reader, &mut out)
        .unwrap_or_else(|e| panic!("expanding wing {}: {e}", wing.id));
    dst
}

// `Database`/`MemoryStore` keep their connection private, so a header assertion
// reads the file through its own connection.
fn raw(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path)
        .unwrap_or_else(|e| panic!("opening {} directly: {e}", path.display()))
}

// The schema version a brand-new database is stamped with, derived by creating
// one rather than by importing the crate's constant.
fn fresh_index_schema_version() -> i32 {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fresh.db");
    Database::open(&path).expect("opening fresh index db");
    read_user_version(&raw(&path))
}

fn fresh_memory_schema_version() -> i32 {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("fresh-memory.db");
    MemoryStore::open(&path).expect("opening fresh memory db");
    read_user_version(&raw(&path))
}

fn read_user_version(conn: &rusqlite::Connection) -> i32 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("reading user_version")
}

fn wings_of_kind<'a>(m: &'a Manifest, kind: &str) -> Vec<&'a Wing> {
    m.wings.iter().filter(|w| w.kind == kind).collect()
}

fn unbundle(bundle: &Path, into: &Path) {
    let parent = into.parent().expect("clone target has a parent directory");
    let status = git_command(parent)
        .args(["clone", "--quiet"])
        .arg(bundle)
        .arg(into)
        .status()
        .expect("running git clone on the corpus bundle");
    assert!(status.success(), "git clone of {} failed", bundle.display());
    // `git clone` of a bundle brings the branches but leaves notes behind:
    // refs/notes/* is outside the default refspec. The bundle was written
    // before the rename, so its notes are on the old ref; fetching them onto
    // the ref this binary reads is exactly what the user-data migration does.
    let status = git_command(into)
        .args([
            "fetch",
            "--quiet",
            "origin",
            "refs/notes/spelunk:refs/notes/inkentry",
        ])
        .status()
        .expect("fetching notes ref from the corpus bundle");
    assert!(status.success(), "fetching notes ref failed");
}

#[test]
#[serial_test::serial]
fn the_corpus_is_not_empty_and_every_wing_is_present() {
    let m = manifest();
    assert!(
        !m.wings.is_empty(),
        "upgrade corpus manifest lists no wings; the museum test would pass vacuously"
    );
    for wing in &m.wings {
        let path = corpus_root()
            .join("wings")
            .join(&wing.id)
            .join(&wing.artifact);
        assert!(
            path.exists(),
            "wing {} (produced by {}) is listed in the manifest but its artifact {} is missing",
            wing.id,
            wing.producer,
            path.display()
        );

        // An artifact that no longer hashes to what was captured is no longer
        // evidence about the release named in `producer`. The expectations in
        // this manifest were read out of *those* bytes, so silently swapping
        // them (a regenerated wing committed without recapturing, a fixture
        // hand-edited to make a test pass) would leave the suite asserting one
        // artifact's contents against another's.
        assert!(
            !wing.sha256.is_empty(),
            "wing {} has no recorded artifact digest, so nothing ties the \
             expectations below to the bytes they were read from",
            wing.id
        );
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("reading wing {} at {}: {e}", wing.id, path.display()));
        let digest = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, wing.sha256,
            "wing {} no longer matches the artifact its expectations were \
             captured from",
            wing.id
        );
    }
}

// What schema step 12 owes a store the real 1.1.0 binary wrote: every entry
// still there with its status, and the tags and linked files that lived in
// comma-joined columns now present as rows, normalised.
//
// The expected tags and paths are written out here rather than computed, so
// they do not echo whatever today's normaliser happens to do. The raw column
// values are asserted first against the manifest, which was read out of the
// artifact with plain SQL at capture time: a regenerated wing with different
// content fails there, loudly, instead of quietly testing something else.
#[test]
#[serial_test::serial]
fn a_store_written_by_1_1_0_survives_the_move_to_the_current_schema() {
    register_sqlite_vec();
    let m = manifest();
    let wing = wings_of_kind(&m, "memory")
        .into_iter()
        .find(|w| w.id == "memory-v1.1.0-schema-11")
        .expect("the 1.1.0 memory wing is in the manifest");
    let tmp = tempfile::tempdir().unwrap();
    let db = checkout(wing, tmp.path());

    assert_eq!(wing.expect.schema_version, 11);
    assert_eq!(
        read_user_version(&raw(&db)),
        11,
        "the artifact must still be the unmigrated store the release wrote"
    );

    struct Case {
        title: &'static str,
        raw_tags: &'static str,
        raw_files: &'static str,
        tags: &'static [&'static str],
        files: &'static [&'static str],
    }
    let cases = [
        Case {
            title: "Chunk with tree-sitter named nodes",
            raw_tags: "Chunking,chunking,tree_sitter",
            raw_files: "./src/lib.rs",
            tags: &["chunking", "tree-sitter"],
            files: &["src/lib.rs"],
        },
        Case {
            title: "Chunk with tree-sitter and re-window oversized nodes",
            raw_tags: "chunking,Tree Sitter",
            raw_files: "src/lib.rs,src/window.rs",
            tags: &["chunking", "tree-sitter"],
            files: &["src/lib.rs", "src/window.rs"],
        },
        Case {
            title: "Index must stay usable without a network",
            raw_tags: "offline",
            raw_files: "README.md",
            tags: &["offline"],
            files: &["README.md"],
        },
        Case {
            title: "Retired plan for a separate vector store",
            raw_tags: "Storage",
            raw_files: "",
            tags: &["storage"],
            files: &[],
        },
        Case {
            title: "Should manifests allow blank lines",
            raw_tags: "",
            raw_files: "",
            tags: &[],
            files: &[],
        },
    ];
    assert_eq!(wing.expect.raw_tags_and_files.len(), cases.len());
    for case in &cases {
        let title = case.title;
        let captured = wing
            .expect
            .raw_tags_and_files
            .iter()
            .find(|r| r.title == title)
            .unwrap_or_else(|| panic!("the wing no longer holds {title:?}"));
        assert_eq!(captured.tags, case.raw_tags, "raw tags of {title:?}");
        assert_eq!(
            captured.linked_files, case.raw_files,
            "raw files of {title:?}"
        );
    }

    let store = MemoryStore::open(&db).expect("opening a 1.1.0 store must migrate it, not refuse");
    assert_eq!(read_user_version(&raw(&db)), fresh_memory_schema_version());

    let all = store.list(None, 100, true).expect("listing every entry");
    assert_eq!(all.len(), wing.expect.note_count);
    let active = store
        .list(None, 100, false)
        .expect("listing active entries");
    assert_eq!(active.len(), wing.expect.active_note_count);

    for case in &cases {
        let title = case.title;
        let note = all
            .iter()
            .find(|n| n.title == title)
            .unwrap_or_else(|| panic!("{title:?} did not survive the migration"));
        let mut got_tags = note.tags.clone();
        got_tags.sort();
        assert_eq!(got_tags, case.tags, "tags of {title:?}");
        let mut got_files = note.linked_files.clone();
        got_files.sort();
        assert_eq!(got_files, case.files, "linked files of {title:?}");
    }

    let status_of = |title: &str| {
        all.iter()
            .find(|n| n.title == title)
            .map(|n| n.status.clone())
            .unwrap_or_default()
    };
    assert_eq!(status_of(&wing.expect.archived_title), "archived");
    assert_eq!(status_of(&wing.expect.superseded_title), "archived");
    assert_eq!(status_of(&wing.expect.successor_title), "active");

    let by_tag = store
        .search_text("offline", 10, None)
        .expect("full-text search after the migration");
    assert!(
        by_tag
            .iter()
            .any(|n| n.title == "Index must stay usable without a network"),
        "a tag must still be reachable through full-text search once it lives in note_tags"
    );

    // Step 13 (ADR-098 D5/D6): every entry the 1.1.0 store wrote predates
    // origin, so it must read as absent — never a fabricated `unknown`
    // object — and the new `events` table must exist and be empty, since
    // nothing has recorded into it yet.
    for note in &all {
        assert_eq!(
            note.origin, None,
            "{:?} predates origin and must read as absent, not a guess",
            note.title
        );
    }
    assert_eq!(
        store.events_in_window(0, i64::MAX).expect("reading events"),
        Vec::new(),
        "a migrated store's events table starts empty"
    );
}

// The ref carries blobs from three writing eras (legacy single-JSON, multi-line
// JSONL without entity_id, entity-keyed event log) and a current read must
// surface every one of them.
//
// This is the one wing covering a path real data takes. The notes ref is
// renamed in place by the user-data migration rather than exported, so these
// exact blob shapes arrive at this binary's reader.

#[tokio::test]
#[serial_test::serial]
async fn git_notes_reads_every_era_on_the_ref() {
    let m = manifest();
    let notes_wings = wings_of_kind(&m, "git-notes");
    assert!(!notes_wings.is_empty(), "corpus has no git-notes wing");

    for wing in notes_wings {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = checkout(wing, tmp.path());
        let repo = tmp.path().join("repo");
        unbundle(&bundle, &repo);

        let backend = GitNotesBackend::with_root(repo.clone());
        let notes = backend
            .list(None, 500, true, None)
            .await
            .unwrap_or_else(|e| panic!("reading wing {} with the current build: {e}", wing.id));

        let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
        for expected in &wing.expect.era_entries {
            let got = notes
                .iter()
                .find(|n| n.title == expected.title)
                .unwrap_or_else(|| {
                    panic!(
                        "wing {}: era entry {:?} is on the ref but the current \
                         reader missed it; saw {:?}",
                        wing.id, expected.title, titles
                    )
                });
            assert_eq!(
                got.kind, expected.kind,
                "wing {}: entry {:?} came back under the wrong kind",
                wing.id, expected.title
            );
            assert_eq!(
                got.body, expected.body,
                "wing {}: entry {:?} came back with the wrong body",
                wing.id, expected.title
            );
        }

        // Exactly the recorded entries, no more. The 0.9.3 era binary really
        // does write each of its entries twice into the log, so a reader that
        // stopped folding duplicates would hand the user the same decision
        // several times over and every title assertion above would still pass.
        assert_eq!(
            notes.len(),
            wing.expect.era_entries.len(),
            "wing {}: the reader returned {} entries for {} distinct records on \
             the ref; saw {:?}",
            wing.id,
            notes.len(),
            wing.expect.era_entries.len(),
            titles
        );
    }
}

// What schema step 18 owes an index the real 1.1.0 binary wrote: it opens in
// place rather than being rebuilt, keeps every file, chunk and edge row it
// held, and every existing edge gains `target_file` as NULL (unresolved) with
// a re-extraction owed to fill it. The wing carries no vectors (see the corpus
// README), so the embedding count is only held to what was captured.
#[test]
#[serial_test::serial]
fn an_index_written_by_1_1_0_migrates_in_place_to_the_current_schema() {
    register_sqlite_vec();
    let m = manifest();
    let wing = wings_of_kind(&m, "index")
        .into_iter()
        .find(|w| w.id == "index-v1.1.0-schema-17")
        .expect("the 1.1.0 index wing is in the manifest");
    let tmp = tempfile::tempdir().unwrap();
    let path = checkout(wing, tmp.path());

    assert_eq!(wing.expect.schema_version, 17);
    assert_eq!(
        read_user_version(&raw(&path)),
        17,
        "the artifact must still be the unmigrated index the release wrote"
    );
    assert!(
        wing.expect.chunk_count > 0 && wing.expect.graph_edge_count > 0,
        "the wing must hold chunks and edges for their survival to mean anything"
    );

    let db = Database::open(&path).expect("a 1.1.0 index must open");
    assert_eq!(
        db.rebuilt_from(),
        None,
        "the index must migrate in place, not be discarded and rebuilt"
    );
    let stats = db.stats().expect("stats");
    assert_eq!(stats.file_count as usize, wing.expect.file_count);
    assert_eq!(stats.chunk_count as usize, wing.expect.chunk_count);
    assert_eq!(stats.embedding_count as usize, wing.expect.embedding_count);
    assert!(
        db.pass_owed(GRAPH_EDGES_REEXTRACT).unwrap(),
        "the migrated index must owe the re-extraction that fills target_file"
    );
    drop(db);

    let conn = raw(&path);
    assert_eq!(read_user_version(&conn), fresh_index_schema_version());
    let (edges, unresolved): (i64, i64) = conn
        .query_row(
            "SELECT count(*), count(*) FILTER (WHERE target_file IS NULL) FROM graph_edges",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("graph_edges has a target_file column after migrating");
    assert_eq!(
        edges as usize, wing.expect.graph_edge_count,
        "no edge may be lost"
    );
    assert_eq!(unresolved, edges, "every existing edge starts unresolved");

    // Step 19 rebuilds the code full-text index from the chunks it finds.
    let fts_rows: i64 = conn
        .query_row("SELECT count(*) FROM chunks_fts", [], |r| r.get(0))
        .expect("chunks_fts exists after migrating");
    assert_eq!(
        fts_rows as usize, wing.expect.chunk_count,
        "every chunk must be in the rebuilt full-text index"
    );

    // Step 20 flags each chunk exactly as a fresh index would, and removes none.
    let flags: Vec<(String, bool, bool)> = conn
        .prepare(
            "SELECT f.path, f.language, c.node_type, c.name, c.text_only
             FROM chunks c JOIN files f ON f.id = c.file_id",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |r| {
                let path: String = r.get(0)?;
                let language: Option<String> = r.get(1)?;
                let name: Option<String> = r.get(3)?;
                let expected = inkentry_core::indexer::embed_scope::is_text_only(
                    &path,
                    language.as_deref().unwrap_or(""),
                    &r.get::<_, String>(2)?,
                    name.as_deref(),
                );
                Ok((path, r.get(4)?, expected))
            })?
            .collect()
        })
        .expect("chunks has a text_only column after migrating");
    assert_eq!(flags.len(), wing.expect.chunk_count);
    for (path, stored, expected) in flags {
        assert_eq!(stored, expected, "{path}");
    }
}

// ── The corpus has to start collecting again when there is something to collect
//
// The wing list is change-boundary driven, and it once stopped advancing
// without anything noticing: it ended at the last release before `user_version`
// existed and stayed there while four more releases shipped, so nothing had
// ever opened a stamped store and the suite went on passing, because a suite
// can only test the wings it has.
//
// A corpus that holds no wing for a store because none is needed looks exactly
// like that failure. What makes the two distinguishable is this assertion: it
// fires whenever a store's schema version moves past what the corpus was last
// checked against, and asks whether a shipped version of that store has to
// cross the move.
//
// The numbers below are an acknowledgement, not a derivation. Deriving them
// from the crate constants would make the check tautological.
//
// Memory 11 -> 12: yes to the question below. 1.0 and 1.1 shipped writing 11,
// and step 12 migrates that store in place. The `memory-v1.1.0-schema-11` wing
// and `a_store_written_by_1_1_0_survives_the_move_to_the_current_schema` are
// the answer.
//
// Index 17 -> 18: yes as well. 1.0 and 1.1 shipped writing index.db at 17, and
// step 18 migrates it in place instead of rebuilding it, so its embeddings are
// kept. The `index-v1.1.0-schema-17` wing and
// `an_index_written_by_1_1_0_migrates_in_place_to_the_current_schema` are the
// answer.
//
// Index 18 -> 19: no. No release wrote 18; the same 1.1.0 wing climbs through
// both steps, and the test above checks what step 19 rebuilds.
//
// Index 19 -> 20: no. No release wrote 19; the 1.1.0 wing climbs through
// step 20 too, and the same test checks that it flags without discarding.
//
// Memory 12 -> 13: no. Schema 12 (tags and linked files as rows) has not
// shipped in a release; no user holds a released binary's memory.db stamped
// 12, so there is nothing a released store needs to survive moving to 13 that
// the `memory-v1.1.0-schema-11` wing does not already cover: it climbs the
// whole ladder, 11 through the current version, including this step.
// `a_store_written_by_1_1_0_survives_the_move_to_the_current_schema` gained
// assertions for what step 13 adds (events exists and is empty; origin reads
// as absent) rather than a new wing.
const CORPUS_COVERS_INDEX_SCHEMA: i32 = 20;
const CORPUS_COVERS_MEMORY_SCHEMA: i32 = 13;

#[test]
#[serial_test::serial]
fn a_schema_version_that_advances_past_the_corpus_fails_here() {
    register_sqlite_vec();
    let m = manifest();

    for (store, kind, constant, current, covered) in [
        (
            "index.db",
            "index",
            "CORPUS_COVERS_INDEX_SCHEMA",
            fresh_index_schema_version(),
            CORPUS_COVERS_INDEX_SCHEMA,
        ),
        (
            "memory.db",
            "memory",
            "CORPUS_COVERS_MEMORY_SCHEMA",
            fresh_memory_schema_version(),
            CORPUS_COVERS_MEMORY_SCHEMA,
        ),
    ] {
        assert_eq!(
            current,
            covered,
            "\n\
             The {store} schema version is now {current}. The upgrade corpus was last checked \
             against version {covered}, and holds {held} wing(s) for this store.\n\
             \n\
             Answer one question, then set {constant} in {file} to {current}:\n\
             \n\
             Has a release shipped that writes version {covered}, and does a user's {store} at \
             that version have to survive the move to {current}?\n\
             \n\
             If yes, this corpus is now the thing standing between that migration and their \
             data, and it has nothing in it. Capture a wing from that release: append to the \
             WINGS table in scripts/upgrade-corpus/generate.sh with the boundary reasoning in \
             the comment above it, pin the release asset in \
             scripts/upgrade-corpus/checksums.txt, run the script, and add a test here that \
             opens the wing and asserts its rows survive.\n\
             \n\
             If no — because the store is rebuilt rather than migrated, or because no release \
             wrote {covered} — then there is no artifact to capture and none is wanted. Record \
             the new version and say which of those two it was.\n\
             \n\
             Do not delete this assertion. The corpus holding nothing is correct today and is \
             indistinguishable, from the inside, from the corpus having quietly stopped.\n",
            held = wings_of_kind(&m, kind).len(),
            file = "crates/inkentry-cli/tests/upgrade_corpus.rs",
        );
    }
}
