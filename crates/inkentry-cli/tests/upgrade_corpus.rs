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
// The corpus holds one wing, and the reason it holds only one is the point.
//
// A wing earns its place by covering a path a real user's data actually takes.
// Neither local database is such a path: `index.db` is not carried at all (the
// user reindexes) and `memory.db` is exported to a portable dump and imported
// into a store this binary creates, so no database written by an earlier
// product is ever opened in place. Wings for those were archaeology, and they
// went with the migration ladders they were defending.
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

use inkentry_core::storage::{Database, GitNotesBackend, MemoryBackend, MemoryStore};
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

// ── The corpus has to start collecting again when there is something to collect
//
// The wing list is change-boundary driven, and it once stopped advancing
// without anything noticing: it ended at the last release before `user_version`
// existed and stayed there while four more releases shipped, so nothing had
// ever opened a stamped store and the suite went on passing, because a suite
// can only test the wings it has.
//
// It now holds no database wing at all, which is correct and is also exactly
// the state that failure looked like. What makes the two distinguishable is
// this assertion. No released version of this product has database data that
// crosses into a newer one: `index.db` is rebuilt by reindexing and `memory.db`
// crosses as a portable dump. The first release to break that is the first one
// whose databases belong in here.
//
// The numbers below are an acknowledgement, not a derivation. Deriving them
// from the crate constants would make the check tautological.
const CORPUS_COVERS_INDEX_SCHEMA: i32 = 16;
const CORPUS_COVERS_MEMORY_SCHEMA: i32 = 11;

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
