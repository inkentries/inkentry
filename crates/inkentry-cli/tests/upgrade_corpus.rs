// Upgrade corpus ("DB museum"): open artifacts written by real released
// binaries with the current build and assert the upgrade preserves them.
//
// Every wing under `fixtures/upgrade-corpus/wings/` was produced by an actual
// downloaded release, not by constructing an old shape by hand. The expected
// values in MANIFEST.json were read out of each artifact at capture time with
// plain SQL, before any current-binary code touched it, so they are an
// independent record of what the old binary wrote rather than an echo of what
// today's migrations happen to produce.
//
// Regenerate with scripts/upgrade-corpus/generate.sh.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use inkentry_core::registry::Registry;
use inkentry_core::storage::{Database, GitNotesBackend, MemoryBackend, MemoryStore};
use inkentry_core::test_support::git_command;
use serde::Deserialize;

// sqlite-vec is registered process-globally, before any connection is opened.
// Without it every vec0 table in the corpus fails to load and the row-count
// assertions would be reading an error, not an empty table.
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
    // `PRAGMA user_version` as the capturing release left it. 0 marks a wing
    // from before anything stamped that store; anything higher is a wing whose
    // header a current build can read. The distinction decides what happens to
    // the artifact — which index migrations run, and which of the two memory
    // refusals the user is given — and no row count reveals it, which is why
    // the corpus records it.
    #[serde(default)]
    schema_version: i32,
    #[serde(default)]
    file_count: i64,
    #[serde(default)]
    chunk_count: i64,
    #[serde(default)]
    embedding_count: i64,
    #[serde(default)]
    graph_edge_count: i64,
    // "int8" wings keep their vectors across the upgrade; a "float768" wing
    // must lose them, because mixed-dimension vectors can never be compared.
    #[serde(default)]
    vector_storage: String,
    #[serde(default)]
    fts_query: String,
    #[serde(default)]
    fts_expect_path: String,
    #[serde(default)]
    note_count: i64,
    #[serde(default)]
    project_count: i64,
    #[serde(default)]
    dep_count: i64,
    #[serde(default)]
    era_entries: Vec<EraEntry>,
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
// opening a database runs migrations, which would otherwise rewrite the
// checked-in fixture and destroy the very thing under test on the first run.
//
// Artifacts are stored gzipped because a captured database is mostly the vec0
// extension's preallocated vector chunk, and that is zeros.
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

// `Database`/`MemoryStore` keep their connection private, so the header and
// schema assertions read the file through their own connection. Callers open
// this only after the typed handle has been dropped.
fn raw(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path)
        .unwrap_or_else(|e| panic!("opening {} directly: {e}", path.display()))
}

// The schema version a brand-new DB is stamped with, derived by creating one
// rather than by importing the crate's constant: an upgraded field DB must land
// on exactly the version a fresh install produces.
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

fn vector_column_type(conn: &rusqlite::Connection, table: &str) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![table],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_else(|e| panic!("reading {table} schema: {e}"))
}

// Callers pass `<name>_rowids` for a vector count: a vec0 virtual table cannot
// be counted directly, but the shadow table the extension maintains alongside
// it is an ordinary table and has one row per stored vector.
fn row_count(conn: &rusqlite::Connection, table: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM {table}");
    conn.query_row(&sql, [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("counting {table}: {e}"))
}

#[allow(dead_code)]
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    let sql = format!("SELECT count(*) FROM pragma_table_info('{table}') WHERE name = ?1");
    conn.query_row(&sql, rusqlite::params![column], |r| r.get::<_, i64>(0))
        .unwrap_or_else(|e| panic!("probing {table}.{column}: {e}"))
        > 0
}

// The two SQLite header fields that move on any write transaction: the file
// change counter (bytes 24..28) and the version-valid-for number (92..96).
const CHANGE_COUNTER: std::ops::Range<usize> = 24..28;
const VERSION_VALID_FOR: std::ops::Range<usize> = 92..96;

// Fold the WAL back into the main file, then return its bytes with those two
// counters masked out.
//
// Checkpointing matters because the idempotency check compares files, and a
// write that only ever reached the -wal sidecar would otherwise read as
// "nothing changed".
//
// Masking matters because the two stores differ in whether a redundant open
// takes a write transaction at all, and that difference is not what is being
// measured here. `Database::open` returns before any write once the header
// already reads the current version, so an index wing comes out of a second
// open byte-identical even unmasked. `MemoryStore::open` runs the entity-id
// backfill and unique-index promotion on every open regardless of version, so
// a memory wing takes a write that settles on identical content and moves only
// these two counters. Measured, not assumed: with no mask at all the index
// wings differ at no offset and the memory wings differ at exactly bytes 27
// and 95, the low bytes of these two fields.
//
// So this tolerates a write that changed no content, and nothing else. Every
// other byte, including all page content, is compared exactly.
fn content_image(path: &Path) -> Vec<u8> {
    {
        let conn = raw(path);
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .expect("checkpointing the write-ahead log");
    }
    let mut bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    for range in [CHANGE_COUNTER, VERSION_VALID_FOR] {
        if bytes.len() >= range.end {
            bytes[range].fill(0);
        }
    }
    bytes
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
    // refs/notes/* is outside the default refspec. The corpus bundle predates
    // the rename, so its notes live on the old ref; fetch them onto the ref the
    // current binary reads, which is exactly what the user-data migration does.
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

// ── The corpus has to keep up with both stores ──────────────────────────────
//
// `index.db` climbs a migration ladder and `memory.db` no longer does, but the
// two are the same shape of problem here: each is gated by one version
// constant, each advances by editing it, and nothing used to connect either
// edit to this corpus. So the wing list stopped at the last release before
// `user_version` existed and stayed there while four more releases shipped:
// every wing was captured at version 0, no wing had ever exercised the stamped
// path, and the suite went on passing, because a suite can only test the wings
// it has. A stamped store is not an edge case — after those four releases it is
// what every user has.
//
// The numbers below are an acknowledgement, not a derivation. Deriving them
// from the crate constants would make the check tautological. Deriving them
// from the wings would make it fail permanently whenever the build legitimately
// runs ahead of the newest release — which is where both sit: index.db is at 16
// against a newest release of 15, and memory.db is at 11 against a released
// range that ends, permanently, at 10.
const CORPUS_COVERS_INDEX_SCHEMA: i32 = 16;
const CORPUS_COVERS_MEMORY_SCHEMA: i32 = 11;

// The highest `user_version` any released binary stamped into a `memory.db`
// (0.9.6 wrote 9; 0.9.7 and 0.9.8 wrote 10). Unlike the constant above, this
// one is finished: the product that wrote those stamps has shipped its last
// release, so no artifact can ever carry a higher one. The corpus has to hold a
// wing at exactly this stamp, because that is the store most users are holding
// when they first meet the refusal — and nothing else here would notice if it
// were dropped, since the older stamped wing satisfies every other check.
const NEWEST_RELEASED_MEMORY_STAMP: i32 = 10;

fn newest_wing<'a>(m: &'a Manifest, kind: &str) -> &'a Wing {
    wings_of_kind(m, kind)
        .into_iter()
        .max_by_key(|w| w.expect.schema_version)
        .unwrap_or_else(|| panic!("the corpus has no {kind} wing"))
}

// One store's coverage claim: the version last checked, what to do when the
// build moves past it, and what each of its two capture eras is there to
// exercise. The remedies differ because the stores do: an index.db behind the
// releases is fixed by capturing a newer one, and a memory.db never can be.
struct StoreCoverage {
    store: &'static str,
    kind: &'static str,
    constant: &'static str,
    current: i32,
    covered: i32,
    remedy: String,
    stamped_route: &'static str,
    unstamped_route: &'static str,
    // Only set where the released range is closed, so a fixed stamp is a
    // requirement rather than a moving target.
    newest_released_stamp: Option<i32>,
}

// This tripwire lives in the corpus test rather than in a file of its own or a
// CI step, for three reasons. It reads the manifest and probes a fresh
// database, which is this file's machinery and nothing else's. It has to fire
// in the same run as the tests it protects: the moment worth interrupting is
// the one where someone bumps a schema constant and runs this suite, not a
// separate CI stage they read later with the reasoning gone. And it needs no
// CI wiring to reach the pull requests that matter, because
// .github/workflows/upgrade-corpus.yml already runs this suite on any change
// under crates/inkentry-core/src/storage/ — which is where both constants live.
#[test]
#[serial_test::serial]
fn a_schema_version_that_advances_past_the_corpus_fails_here() {
    register_sqlite_vec();
    let m = manifest();

    let index_newest = newest_wing(&m, "index");
    let memory_newest = newest_wing(&m, "memory");
    let index_current = fresh_index_schema_version();
    let memory_current = fresh_memory_schema_version();

    for store in [
        StoreCoverage {
            store: "index.db",
            kind: "index",
            constant: "CORPUS_COVERS_INDEX_SCHEMA",
            current: index_current,
            covered: CORPUS_COVERS_INDEX_SCHEMA,
            remedy: format!(
                "1. If a release has shipped that writes user_version above {stamp}, the \
                 corpus is genuinely behind. Add a wing for it: append to the WINGS table in \
                 scripts/upgrade-corpus/generate.sh, put the boundary reasoning in the \
                 comment above that table, pin the release asset in \
                 scripts/upgrade-corpus/checksums.txt, and re-run the script.\n\
                 \n\
                 2. If no release writes that version yet, then no artifact can exist for it, \
                 and `{wing}` is still the newest index a user can be holding. Check that \
                 opening it with this build lands on {current} and keeps its rows — the tests \
                 in this file do exactly that — and record the new version.",
                stamp = index_newest.expect.schema_version,
                wing = index_newest.id,
                current = index_current,
            ),
            stamped_route: "the route a current field database takes: its header is trusted \
                            and only the migration steps above it run",
            unstamped_route: "the route where the version has to be inferred from table \
                              shapes because nothing stamped it",
            newest_released_stamp: None,
        },
        StoreCoverage {
            store: "memory.db",
            kind: "memory",
            constant: "CORPUS_COVERS_MEMORY_SCHEMA",
            current: memory_current,
            covered: CORPUS_COVERS_MEMORY_SCHEMA,
            remedy: format!(
                "No new wing can answer this one. The product that stamped memory.db below \
                 this build has shipped its last release, so {stamp} is the highest stamp any \
                 artifact will ever carry and `{wing}` stays the newest store a user can be \
                 holding.\n\
                 \n\
                 What moved is this build's own shape — and that is the boundary the refusal \
                 is drawn at. Check that every memory wing is still refused, still names the \
                 way across, and is still left with its rows intact \
                 (`every_memory_wing_is_refused_rather_than_opened_in_place` does exactly \
                 that), then record the new version.\n\
                 \n\
                 If a wing has started opening instead, do not record it. A store from the \
                 older product has just been opened in place over rows whose identity, \
                 supersede column and edge endpoints are every one of them a different shape.",
                stamp = memory_newest.expect.schema_version,
                wing = memory_newest.id,
            ),
            stamped_route: "the refusal that reads a stamp and can tell the user which \
                            version it found",
            unstamped_route: "the refusal that has only the tables to go on, because the \
                              store predates the stamp entirely",
            newest_released_stamp: Some(NEWEST_RELEASED_MEMORY_STAMP),
        },
    ] {
        let StoreCoverage {
            store: name,
            kind,
            constant,
            current,
            covered,
            remedy,
            stamped_route,
            unstamped_route,
            newest_released_stamp,
        } = store;
        let newest = newest_wing(&m, kind);

        assert_eq!(
            current,
            covered,
            "\n\
             The {name} schema version is now {current}. The upgrade corpus was last checked \
             against version {covered}.\n\
             \n\
             Its newest {kind} wing is `{wing}`, produced by release {producer}, whose store \
             was captured stamped at user_version {stamp}. Nothing in this corpus was written \
             by a binary that knows about version {current}.\n\
             \n\
             Do this, then set {constant} in {file} to {current}:\n\
             \n\
             {remedy}\n\
             \n\
             Do not delete this assertion. It is the only thing that notices when the corpus \
             stops covering the versions users actually have.\n",
            wing = newest.id,
            producer = newest.producer,
            stamp = newest.expect.schema_version,
            file = "crates/inkentry-cli/tests/upgrade_corpus.rs",
        );

        // Both capture eras have to stay represented. Dropping either is how
        // this coverage was lost the first time, and it is invisible in every
        // other test here: the wings that remain all pass.
        let wings = wings_of_kind(&m, kind);
        assert!(
            wings.iter().any(|w| w.expect.schema_version > 0),
            "no {kind} wing was captured with a stamped user_version, so nothing exercises \
             {stamped_route}"
        );
        assert!(
            wings.iter().any(|w| w.expect.schema_version == 0),
            "no {kind} wing was captured before anything stamped a version, so nothing \
             exercises {unstamped_route}"
        );

        if let Some(released) = newest_released_stamp {
            assert!(
                wings.iter().any(|w| w.expect.schema_version == released),
                "no {kind} wing is stamped at {released}, the highest version any release \
                 ever wrote. That is the store most users are holding, and every other check \
                 in this file is satisfied by the older stamped wing, so dropping it costs \
                 the coverage silently"
            );
        }
    }
}

// Criterion 1: every wing opens, migrates, and keeps its rows and content.

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

#[test]
#[serial_test::serial]
fn every_index_wing_migrates_with_its_rows_and_content_intact() {
    register_sqlite_vec();
    let m = manifest();
    let expected_version = fresh_index_schema_version();
    let index_wings = wings_of_kind(&m, "index");
    assert!(!index_wings.is_empty(), "corpus has no index.db wing");

    for wing in index_wings {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());

        // The era claim, checked before the upgrade can rewrite it. A wing
        // recorded as unstamped that arrives already stamped (a fixture opened
        // by a current build and committed back) exercises the trusted-header
        // path while the manifest still says it covers the inference path, and
        // every assertion below passes either way.
        assert_eq!(
            read_user_version(&raw(&db_path)),
            wing.expect.schema_version,
            "wing {}: the captured artifact is not stamped at the schema \
             version it is recorded as",
            wing.id
        );

        let db = Database::open(&db_path)
            .unwrap_or_else(|e| panic!("opening wing {} with the current build: {e}", wing.id));

        assert_eq!(
            read_user_version(&raw(&db_path)),
            expected_version,
            "wing {} did not land on the schema version a fresh install produces",
            wing.id
        );

        let stats = db.stats().expect("reading index stats");
        assert_eq!(
            stats.file_count, wing.expect.file_count,
            "wing {}: file count changed across the upgrade",
            wing.id
        );
        assert_eq!(
            stats.chunk_count, wing.expect.chunk_count,
            "wing {}: chunk count changed across the upgrade",
            wing.id
        );

        let hits = db
            .search_text(&wing.expect.fts_query, 10)
            .expect("full-text search over the upgraded index");
        assert!(
            hits.iter()
                .any(|h| h.file_path == wing.expect.fts_expect_path),
            "wing {}: FTS for {:?} lost the pre-existing chunk from {}; got {:?}",
            wing.id,
            wing.expect.fts_query,
            wing.expect.fts_expect_path,
            hits.iter().map(|h| &h.file_path).collect::<Vec<_>>()
        );
        assert!(
            hits.iter()
                .any(|h| h.content.contains(&wing.expect.fts_query)),
            "wing {}: chunk text no longer contains {:?} after the upgrade",
            wing.id,
            wing.expect.fts_query
        );

        drop(db);
        let conn = raw(&db_path);

        // The code graph is a whole subsystem the file and chunk counts say
        // nothing about: emptying graph_edges leaves both of them intact.
        assert_eq!(
            row_count(&conn, "graph_edges"),
            wing.expect.graph_edge_count,
            "wing {}: the code graph lost edges across the upgrade",
            wing.id
        );

        // A wing already storing int8 vectors must keep every one of them. The
        // dimension upgrade is allowed to discard 768-dimension vectors and
        // only those; a detection bug that rebuilt an int8 table as well would
        // silently cost the user their whole embedding index, and re-embedding
        // is the single most expensive thing this tool asks of them.
        if wing.expect.vector_storage != "float768" {
            assert_eq!(
                row_count(&conn, "embeddings_rowids"),
                wing.expect.embedding_count,
                "wing {}: vectors were discarded from an index that was already \
                 int8, so the whole index would have to be re-embedded",
                wing.id
            );
        }
    }
}

#[test]
#[serial_test::serial]
fn every_memory_wing_is_refused_rather_than_opened_in_place() {
    register_sqlite_vec();
    let m = manifest();
    let memory_wings = wings_of_kind(&m, "memory");
    assert!(!memory_wings.is_empty(), "corpus has no memory.db wing");

    for wing in memory_wings {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());

        // Confirm the artifact really is an older-era store before asserting
        // that it is refused, so a pre-migrated fixture cannot make this pass
        // for the wrong reason.
        let before = raw(&db_path);
        assert!(
            row_count(&before, "notes") > 0,
            "wing {}: the captured artifact holds no entries, so refusing it proves nothing",
            wing.id
        );
        assert_eq!(
            read_user_version(&before),
            wing.expect.schema_version,
            "wing {}: the captured artifact is not stamped at the schema \
             version it is recorded as",
            wing.id
        );
        drop(before);

        // Memory stores no longer climb a migration ladder: identity, the
        // supersede column and both edge endpoints changed shape, and the
        // crossing is an export/import rather than an in-place conversion.
        // Opening one anyway would half-apply a schema over real entries.
        let msg = match MemoryStore::open(&db_path) {
            Ok(_) => panic!(
                "wing {}: a store from an older product must not be opened in place",
                wing.id
            ),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("inkentry import"),
            "wing {}: the refusal must name the way across, got: {msg}",
            wing.id
        );

        // A stamped store and an unstamped one reach the same refusal down
        // different branches, and only the stamped branch can say which
        // version it found. That number is the one thing distinguishing a
        // store this product could once open from one it never could, so it
        // has to reach the user rather than being computed and dropped.
        if wing.expect.schema_version > 0 {
            assert!(
                msg.contains(&format!("schema version {}", wing.expect.schema_version)),
                "wing {}: the refusal dropped the stamp it found, got: {msg}",
                wing.id
            );
        } else {
            assert!(
                !msg.contains("schema version"),
                "wing {}: the refusal claims a schema version for an artifact that \
                 was never stamped, got: {msg}",
                wing.id
            );
        }

        // Refused, not damaged: the artifact must still hold every entry it
        // arrived with, so the export half has something to read.
        let after = raw(&db_path);
        assert_eq!(
            row_count(&after, "notes"),
            wing.expect.note_count,
            "wing {}: a refused open must leave the store untouched",
            wing.id
        );
    }
}

// The project paths exactly as the capturing release wrote them. Read before
// the current build opens the file, so it is a record of the artifact rather
// than of what today's migrations produce.
fn captured_project_paths(conn: &rusqlite::Connection) -> BTreeMap<i64, (String, String)> {
    let mut stmt = conn
        .prepare("SELECT id, root_path, db_path FROM projects")
        .expect("preparing the captured-paths query");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, String>(2)?),
            ))
        })
        .expect("querying captured project paths");
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .expect("reading captured project paths")
}

#[test]
#[serial_test::serial]
fn every_registry_wing_keeps_its_projects_and_dependency_links() {
    let m = manifest();
    let registry_wings = wings_of_kind(&m, "registry");
    assert!(!registry_wings.is_empty(), "corpus has no registry.db wing");

    for wing in registry_wings {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());
        let captured = captured_project_paths(&raw(&db_path));

        // Nothing below can show that a path survived if the artifact never
        // held a usable one. `starts_with` matches whole components and reads
        // the string alone, so it says the same thing on every platform.
        for (id, (root, db)) in &captured {
            assert!(
                !root.is_empty() && Path::new(db).starts_with(Path::new(root)),
                "wing {}: project {} was captured without a database under its \
                 own root, so the comparison below has nothing to preserve",
                wing.id,
                id
            );
        }

        // INKENTRY_REGISTRY_DIR is the only way to point Registry::open at a
        // temp copy; dirs::config_dir() is not redirectable on every platform.
        unsafe { std::env::set_var("INKENTRY_REGISTRY_DIR", tmp.path()) };
        let registry = Registry::open()
            .unwrap_or_else(|e| panic!("opening wing {} with the current build: {e}", wing.id));

        let projects = registry
            .all_projects()
            .expect("listing registered projects");
        assert_eq!(
            projects.len() as i64,
            wing.expect.project_count,
            "wing {}: registered projects changed across the upgrade",
            wing.id
        );

        // Counting rows says nothing about what is in them, and a registry row
        // whose paths have been mangled is worse than a missing one: the
        // project still lists, and then every command against it looks at the
        // wrong place on disk.
        //
        // The check is equality with the captured bytes, not a shape test.
        // These paths belong to the machine the wing was captured on, so
        // `is_absolute` and every other host-OS path predicate answers for the
        // runner rather than for the artifact: a POSIX path is not absolute to
        // a Windows host, whatever the migration did to it. Equality is the
        // same question everywhere, and it is the stronger one anyway, since a
        // path rewritten to some other absolute path is still mangled.
        for project in &projects {
            let (root, db) = captured.get(&project.id).unwrap_or_else(|| {
                panic!(
                    "wing {}: project {} is not one of the rows the capturing \
                     release wrote",
                    wing.id, project.id
                )
            });
            assert_eq!(
                project.root_path,
                Path::new(root),
                "wing {}: project {} came back with a rewritten root path",
                wing.id,
                project.id
            );
            assert_eq!(
                project.db_path,
                Path::new(db),
                "wing {}: project {} came back with a rewritten database path",
                wing.id,
                project.id
            );
            assert!(
                project.registered_at > 0,
                "wing {}: project {} lost its registration timestamp",
                wing.id,
                project.id
            );
        }

        let mut total_deps = 0i64;
        for project in &projects {
            for dep in registry.get_deps(project.id).expect("reading deps") {
                total_deps += 1;
                assert!(
                    projects.iter().any(|p| p.id == dep.id),
                    "wing {}: project {} depends on id {}, which is not a \
                     registered project; the link outlived its target",
                    wing.id,
                    project.id,
                    dep.id
                );
                assert_ne!(
                    dep.id, project.id,
                    "wing {}: project {} now depends on itself",
                    wing.id, project.id
                );
            }
        }
        assert_eq!(
            total_deps, wing.expect.dep_count,
            "wing {}: dependency links changed across the upgrade",
            wing.id
        );

        unsafe { std::env::remove_var("INKENTRY_REGISTRY_DIR") };
    }
}

// Criterion 2 (git-notes half): the ref carries blobs from three writing eras
// (legacy single-JSON, multi-line JSONL without entity_id, entity-keyed event
// log) and a current read must surface every one of them.

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

// Criterion 3: a FLOAT[768] index is rebuilt empty as INT8[896] so a re-embed is
// required, rather than left holding vectors that cannot be compared with the
// ones the current embedder produces.

#[test]
#[serial_test::serial]
fn a_float768_wing_is_rebuilt_empty_as_int8_rather_than_serving_mixed_vectors() {
    register_sqlite_vec();
    let m = manifest();
    let float_wings: Vec<&Wing> = m
        .wings
        .iter()
        .filter(|w| w.kind == "index" && w.expect.vector_storage == "float768")
        .collect();
    assert!(
        !float_wings.is_empty(),
        "corpus has no 768-dimension index wing, so the dimension-upgrade path is untested"
    );

    for wing in float_wings {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());

        // The captured artifact really is a 768-dimension index before the
        // current build touches it; without this the assertions below could
        // pass on an already-upgraded fixture.
        {
            let raw = rusqlite::Connection::open(&db_path).expect("opening the raw fixture");
            assert!(
                vector_column_type(&raw, "embeddings").contains("FLOAT[768]"),
                "wing {} was captured as a 768-dimension index but is not one",
                wing.id
            );
            assert!(
                wing.expect.embedding_count > 0,
                "wing {} has no stored vectors, so it cannot show that they are discarded",
                wing.id
            );
        }

        let db = Database::open(&db_path)
            .unwrap_or_else(|e| panic!("opening wing {} with the current build: {e}", wing.id));
        let upgraded = raw(&db_path);

        assert!(
            vector_column_type(&upgraded, "embeddings").contains("INT8[896]"),
            "wing {}: the 768-dimension vector table was not rebuilt as int8[896]",
            wing.id
        );
        assert_eq!(
            row_count(&upgraded, "embeddings_rowids"),
            0,
            "wing {}: stale 768-dimension vectors survived the rebuild and would be \
             ranked against 896-dimension query vectors",
            wing.id
        );
        assert_eq!(
            db.stats().expect("reading index stats").chunk_count,
            wing.expect.chunk_count,
            "wing {}: the dimension upgrade discarded chunks, not just vectors; \
             a re-embed would have nothing to rebuild from",
            wing.id
        );
    }
}

// Criterion 5: the second open is a no-op. Byte equality is the strongest
// statement of that and catches a migration that rewrites rows every time.

#[test]
#[serial_test::serial]
fn upgrading_a_wing_twice_changes_nothing_the_second_time() {
    register_sqlite_vec();
    let m = manifest();

    // Memory wings are excluded: a memory store from an earlier product is
    // refused rather than opened in place (see
    // `every_memory_wing_is_refused_rather_than_opened_in_place`), so there is
    // no in-place upgrade whose idempotency could be measured.
    for wing in m
        .wings
        .iter()
        .filter(|w| w.kind != "git-notes" && w.kind != "memory")
    {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = checkout(wing, tmp.path());

        open_for_kind(&wing.kind, &db_path, tmp.path());
        let after_first = content_image(&db_path);

        open_for_kind(&wing.kind, &db_path, tmp.path());
        let after_second = content_image(&db_path);

        assert_eq!(
            after_first.len(),
            after_second.len(),
            "wing {}: a second open changed the database size",
            wing.id
        );
        assert!(
            after_first == after_second,
            "wing {}: a second open rewrote the database; the upgrade is not idempotent",
            wing.id
        );
    }
}

fn open_for_kind(kind: &str, db_path: &Path, dir: &Path) {
    match kind {
        "index" => {
            Database::open(db_path).expect("opening index wing");
        }
        "memory" => {
            MemoryStore::open(db_path).expect("opening memory wing");
        }
        "registry" => {
            unsafe { std::env::set_var("INKENTRY_REGISTRY_DIR", dir) };
            Registry::open().expect("opening registry wing");
            unsafe { std::env::remove_var("INKENTRY_REGISTRY_DIR") };
        }
        other => panic!("no opener wired up for wing kind {other:?}"),
    }
}

// Criterion 4: a pinned old release opening a database the current build has
// already upgraded. The behaviour has to be defined and asserted rather than
// assumed, because a user who upgrades, then runs an older binary still on
// their PATH, hits exactly this.
//
// Measured behaviour, against v0.9.2, v0.9.3 and v0.9.5: a clean read, never a
// refusal. The old binary exits 0, reports the correct file/chunk/embedding
// counts, lists the memory entries, and returns full-text search hits, and no
// row is lost.
//
// One wrinkle the corpus surfaced, which is why the version is asserted
// separately from the data: a release whose own schema version is *below* the
// current one re-stamps `PRAGMA user_version` down to its own on close. v0.9.3
// rewinds an index.db from 15 to 14. v0.9.2 pre-dates the header entirely and
// never stamps; v0.9.5 stamps the same value it finds. The rewind loses no
// data, and the next current-build open heals it: the steps above the rewound
// version are individually idempotent, so they re-run as no-ops and re-stamp
// the current version. That heal is asserted here rather than assumed, because
// it is the only thing standing between a rewind and a re-run of migrations
// against a schema that already has them.
//
// Ignored by default because it needs a downloaded release. CI runs it with
// INKENTRY_OLD_BINARY pointing at one; run it locally the same way.

fn old_binary() -> PathBuf {
    let raw = std::env::var("INKENTRY_OLD_BINARY").expect(
        "INKENTRY_OLD_BINARY must point at a pinned released inkentry binary; \
         scripts/upgrade-corpus/generate.sh downloads one into its cache",
    );
    let path = PathBuf::from(raw);
    assert!(
        path.is_file(),
        "INKENTRY_OLD_BINARY does not exist: {}",
        path.display()
    );
    path
}

// A project directory holding both databases from the corpus, already upgraded
// to the current schema by the current build.
fn upgraded_project(tmp: &Path) -> PathBuf {
    let m = manifest();
    let project = tmp.join("project");
    let dot = project.join(".inkentry");
    std::fs::create_dir_all(&dot).unwrap();

    let index_wing = wings_of_kind(&m, "index")
        .into_iter()
        .find(|w| w.expect.vector_storage != "float768")
        .expect("corpus has no int8 index wing to upgrade");
    let staged_index = checkout(index_wing, tmp);
    std::fs::rename(&staged_index, dot.join("index.db")).unwrap();

    // A git repo, because the CLI resolves a project from one.
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "corpus@inkentry.invalid"],
        vec!["config", "user.name", "corpus"],
    ] {
        let ok = git_command(&project)
            .args(&args)
            .status()
            .expect("running git")
            .success();
        assert!(ok, "git {args:?} failed");
    }
    std::fs::write(project.join("seed.txt"), "corpus\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "--quiet", "-m", "seed"]] {
        let ok = git_command(&project)
            .args(&args)
            .status()
            .expect("running git")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    register_sqlite_vec();
    Database::open(&dot.join("index.db")).expect("upgrading the index wing to current");
    // A corpus memory wing cannot be staged here: an earlier product's store is
    // refused, never opened in place. The old binary is being asked to read a
    // CURRENT memory.db, so the current build writes one.
    let store = MemoryStore::open(&dot.join("memory.db")).expect("creating a current memory wing");
    store
        .add_note(
            "decision",
            MEMORY_ENTRY_TITLE,
            "captured so the old binary has an entry to list",
            &[],
            &[],
            None,
            None,
        )
        .expect("seeding the memory wing");
    project
}

// The one memory entry `upgraded_project` seeds, which the old binary must
// list back.
const MEMORY_ENTRY_TITLE: &str = "Index must stay usable without a network";

fn table_counts(dot: &Path) -> (i32, i64, i64, i32, i64) {
    let index = raw(&dot.join("index.db"));
    let memory = raw(&dot.join("memory.db"));
    (
        read_user_version(&index),
        index
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
            .unwrap(),
        row_count(&index, "embeddings_rowids"),
        read_user_version(&memory),
        memory
            .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
            .unwrap(),
    )
}

#[test]
#[ignore = "needs a downloaded release binary in INKENTRY_OLD_BINARY"]
#[serial_test::serial]
fn a_pinned_old_binary_reads_a_current_database_cleanly_and_loses_no_data() {
    let bin = old_binary();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");

    // Every identifier below spells the name the PINNED RELEASE shipped under,
    // not the current one, and none of them may be swept into the current
    // spelling: this test drives a real 0.9.x binary, and that binary reads the
    // environment variables, the config directory and the project directory
    // that existed when it was built. Renaming any of them does not make the
    // test more consistent, it makes the old binary miss its config, fall back
    // to the OS keychain, and fail to find the databases the assertions are
    // about — a failure that reads like a migration bug and is not one.
    let legacy_config = home.join(".config").join("spelunk");
    std::fs::create_dir_all(&legacy_config).unwrap();
    std::fs::write(legacy_config.join("config.toml"), "").unwrap();

    let project = upgraded_project(tmp.path());
    let dot = project.join(".inkentry");

    // The pinned release looks for its databases in the project directory it
    // knew. Linked rather than copied, so the assertions below read the exact
    // files the old binary opened.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dot, project.join(".spelunk")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&dot, project.join(".spelunk")).unwrap();

    let before = table_counts(&dot);

    let run = |args: &[&str]| -> std::process::Output {
        std::process::Command::new(&bin)
            .current_dir(&project)
            .args(args)
            // An old binary predates the file secret-store default and would
            // otherwise reach the OS keychain and block on a prompt.
            .env("SPELUNK_SECRET_STORE", "file")
            .env("HOME", &home)
            .env("SPELUNK_CONFIG_DIR", &legacy_config)
            .env("SPELUNK_REGISTRY_DIR", &legacy_config)
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap_or_else(|e| panic!("running the old binary with {args:?}: {e}"))
    };

    let status = run(&["status"]);
    assert!(
        status.status.success(),
        "the old binary refused a current index.db: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_out.contains(&format!("Chunks:     {}", before.1)),
        "the old binary did not report the current index's chunk count; got:\n{status_out}"
    );

    let listing = run(&["memory", "list"]);
    assert!(
        listing.status.success(),
        "the old binary refused a current memory.db: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    assert!(
        String::from_utf8_lossy(&listing.stdout).contains(MEMORY_ENTRY_TITLE),
        "the old binary read a current memory.db but not its entries"
    );

    let search = run(&["search", "parse_manifest", "--mode", "text"]);
    assert!(
        search.status.success(),
        "the old binary failed full-text search on a current index.db: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    assert!(
        String::from_utf8_lossy(&search.stdout).contains("parse_manifest"),
        "the old binary's full-text search lost pre-existing content"
    );

    let after = table_counts(&dot);
    assert_eq!(
        (after.1, after.2, after.4),
        (before.1, before.2, before.4),
        "the old binary lost rows from a database the current build had already \
         upgraded; a read must never cost data"
    );
    assert!(
        after.0 <= before.0 && after.3 <= before.3,
        "the old binary stamped a schema version above the current build's \
         ({after:?} vs {before:?}); a newer build would then skip migrations it \
         has never actually run"
    );

    // The heal. Re-opening with the current build must restore the current
    // version without disturbing anything the old binary left behind.
    Database::open(&dot.join("index.db")).expect("re-opening the index after the old binary");
    MemoryStore::open(&dot.join("memory.db")).expect("re-opening memory after the old binary");
    assert_eq!(
        table_counts(&dot),
        before,
        "re-opening with the current build did not restore the state it had \
         before the old binary ran"
    );
}
