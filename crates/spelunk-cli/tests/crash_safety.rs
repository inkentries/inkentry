// Chaos-engineering drills for the layer *this codebase* owns on top of
// SQLite: our transaction boundaries, our blake3-hash resume/skip logic, our
// multi-DB (index.db / memory.db) consistency, and our concurrent-access
// behaviour. SQLite's own WAL/fsync/B-tree durability is out of scope, so
// every drill here targets a window this codebase controls, not one SQLite
// already guarantees.
//
// Every SIGKILL below is real: the target process is a real `spelunk` child
// spawned via `Command`, parked at a specific write-window by
// `crash_test_hook::pause_at`/`storage::pause_for_crash_test` (env-gated,
// inert for every real invocation), and killed with `Child::kill()`, which
// sends `SIGKILL` on Unix. Nothing here simulates a crash by catching a panic
// or calling `std::process::exit` in-process.

mod plumbing_helpers;

use plumbing_helpers::{mount_health, mount_index_embed, write_project_server_config};
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

const MARKER_TIMEOUT: Duration = Duration::from_secs(30);

/// The `embeddings` table is a `vec0` virtual table, which needs the
/// `sqlite_vec` extension registered before any connection in *this* process
/// can read it - the spawned `spelunk` binary registers it for itself, but a
/// raw `rusqlite::Connection::open` from the test process does not get that
/// for free. Without this, a query against `embeddings` fails and
/// `embedding_count`'s `.unwrap_or(0)` would silently misreport "empty"
/// instead of surfacing the real error.
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

// ── Process plumbing ─────────────────────────────────────────────────────────

/// Build a `spelunk` `Command` isolated from the developer's real keychain,
/// config dir, and git identity, mirroring `plumbing_helpers::spelunk_bin_in`
/// but returning a raw `std::process::Command` so callers get full control
/// over stdio (needed to pipe stdin/stdout for the marker-then-kill protocol
/// below; `assert_cmd::Command` does not expose that).
fn spelunk_command(home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("spelunk"));
    cmd.env("SPELUNK_SECRET_STORE", "file")
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env("SPELUNK_CONFIG_DIR", home.join(".config").join("spelunk"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

/// A child parked at a crash point: stdin/stdout piped and a background
/// thread draining stdout into `stdout_so_far` (so the child never blocks on
/// a full pipe buffer after the marker line, and so a failed assertion can
/// print what the child actually said).
struct PausedChild {
    child: Child,
    stdout_so_far: std::sync::Arc<std::sync::Mutex<String>>,
}

/// Spawn `cmd` with `SPELUNK_TEST_CRASH_POINT=<point>` and block until the
/// child prints the matching `SPELUNK_TEST_CRASH_POINT_REACHED:<point>`
/// marker (see `storage::pause_for_crash_test` / `crash_test_hook::pause_at`),
/// proving it is parked exactly inside the write window under test rather
/// than merely "probably there by now". Panics loudly (with the child's
/// stdout so far) if the marker never arrives, rather than hanging or
/// silently no-opping the drill.
fn spawn_paused_at(mut cmd: Command, point: &str) -> PausedChild {
    cmd.env("SPELUNK_TEST_CRASH_POINT", point)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn spelunk");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Drain stderr too so a chatty child can't deadlock on a full pipe while
    // we wait on the stdout marker.
    std::thread::spawn(move || {
        let mut r = BufReader::new(stderr);
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });

    let buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let buf_writer = buf.clone();
    let marker = format!("SPELUNK_TEST_CRASH_POINT_REACHED:{point}");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(false);
                    return;
                }
                Err(_) => {
                    let _ = tx.send(false);
                    return;
                }
                Ok(_) => {
                    buf_writer.lock().unwrap().push_str(&line);
                    if line.contains(&marker) {
                        let _ = tx.send(true);
                        // Keep draining afterward so the child never blocks
                        // on a full stdout pipe for the rest of its life.
                        loop {
                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => return,
                                Ok(_) => buf_writer.lock().unwrap().push_str(&line),
                            }
                        }
                    }
                }
            }
        }
    });

    let reached = rx.recv_timeout(MARKER_TIMEOUT).unwrap_or(false);
    if !reached {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "child never reached crash point {point:?} within {MARKER_TIMEOUT:?}; stdout so \
             far:\n{}",
            buf.lock().unwrap()
        );
    }
    PausedChild {
        child,
        stdout_so_far: buf,
    }
}

/// SIGKILL the paused child and wait for it to be reaped. Asserts it actually
/// died by signal (not a coincidental clean exit), which would otherwise mean
/// the drill never really tested a crash.
fn kill_and_reap(mut pc: PausedChild) {
    pc.child.kill().expect("SIGKILL the paused child");
    let status = pc.child.wait().expect("reap the killed child");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            status.signal().is_some(),
            "child must have died by signal, not exited on its own (status: {status:?}); stdout \
             so far:\n{}",
            pc.stdout_so_far.lock().unwrap()
        );
    }
    #[cfg(not(unix))]
    let _ = status;
}

/// Release a paused child without crashing it: write a byte to its stdin
/// (unblocking the `read` in `pause_at`/`pause_for_crash_test`) and wait for
/// a normal exit. Used by drills that need a real, held write/lock window but
/// are not themselves testing a kill (e.g. the concurrent-reader drill).
fn release_and_wait(mut pc: PausedChild) -> std::process::ExitStatus {
    {
        let stdin = pc.child.stdin.as_mut().expect("piped stdin");
        let _ = stdin.write_all(b"\n");
    }
    pc.child.wait().expect("wait for released child")
}

// ── DB assertions shared by every drill ──────────────────────────────────────

/// SQLite's own structural guarantee: never violated by a `SIGKILL` at any
/// point, since it is exactly what WAL/journal recovery on the next open
/// exists to uphold. Asserted in every drill as the baseline "reopens clean"
/// check, distinct from (and less interesting than) the product-level
/// invariants asserted alongside it.
fn assert_integrity_ok(db_path: &Path) {
    register_sqlite_vec();
    let conn = Connection::open(db_path).expect("reopen db after crash");
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "SQLite-level corruption after a crash");
}

fn file_hash(conn: &Connection, path: &str) -> Option<String> {
    conn.query_row(
        "SELECT hash FROM files WHERE path = ?1",
        rusqlite::params![path],
        |r| r.get(0),
    )
    .ok()
}

fn chunk_count_for(conn: &Connection, path: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM chunks c JOIN files f ON f.id = c.file_id WHERE f.path = ?1",
        rusqlite::params![path],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn all_file_paths(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT path FROM files").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn embedding_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
        .expect("query embeddings (requires register_sqlite_vec() before opening the connection)")
}

fn page_count(db_path: &Path) -> i64 {
    let conn = Connection::open(db_path).expect("open for page_count");
    conn.query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("read page_count")
}

// ── Drill 1-3: the parse-phase per-file crash window ─────────────────────────
//
// `process_text_file` (parse_phase.rs) commits the file's new blake3 hash via
// `upsert_file` *before* it deletes/inserts that file's chunks - there is no
// transaction spanning the two. A SIGKILL landed between them (pinned here by
// `crash_test_hook::pause_at("after_index_hash_write", path)`) leaves the
// file's `files.hash` already matching its on-disk content while `chunks` for
// it is empty. Drills 1-3 pin exactly that window and its consequences.

struct InterruptedFixture {
    _home: TempDir,
    project: TempDir,
    db_path: PathBuf,
}

/// Three files; the crash point targets `target.py` specifically so the
/// window is pinned regardless of the walk's (unspecified) file order. The
/// other two are asserted only for "fully present or fully absent, never
/// partial" - not for a specific order - since the walk order is not a
/// contract this suite should pin.
fn write_three_file_project(dir: &Path) {
    std::fs::write(dir.join("alpha.py"), "def alpha():\n    return 1\n").unwrap();
    std::fs::write(dir.join("target.py"), "def target():\n    return 2\n").unwrap();
    std::fs::write(dir.join("gamma.py"), "def gamma():\n    return 3\n").unwrap();
}

/// Run `spelunk index`, killing it exactly after `target.py`'s hash commits
/// and before any of its chunks do.
fn crash_mid_target_file() -> InterruptedFixture {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    write_three_file_project(project.path());
    let db_path = project.path().join(".spelunk").join("index.db");

    let mut cmd = spelunk_command(home.path());
    cmd.current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused = spawn_paused_at(cmd, "after_index_hash_write:target.py");
    kill_and_reap(paused);

    InterruptedFixture {
        _home: home,
        project,
        db_path,
    }
}

#[test]
fn interrupted_file_hash_commits_before_its_chunks_pinning_the_real_write_ordering() {
    let f = crash_mid_target_file();
    assert_integrity_ok(&f.db_path);

    let conn = Connection::open(&f.db_path).expect("open db");
    assert!(
        file_hash(&conn, "target.py").is_some(),
        "upsert_file must have committed before the kill (that is the window under test)"
    );
    assert_eq!(
        chunk_count_for(&conn, "target.py"),
        0,
        "the kill landed before any chunk of target.py was written, so it must have none - a \
         nonzero count here would mean the crash point fired too late to test the intended \
         window"
    );

    // Every other file must be fully present or fully absent - never the same
    // half-state target.py is in. Walk order is not pinned, so both outcomes
    // are accepted per file; a partial one is not.
    for path in ["alpha.py", "gamma.py"] {
        match file_hash(&conn, path) {
            None => {} // never reached: fine, that is not the window under test
            Some(_) => assert!(
                chunk_count_for(&conn, path) > 0,
                "{path} has a committed hash but zero chunks - the same half-indexed state as \
                 target.py, on a file the crash point never targeted"
            ),
        }
    }
}

#[test]
fn plain_reindex_does_not_heal_a_hash_current_empty_chunks_file() {
    // Known gap, pinned deliberately: `db.file_hash(path) == hash` short-
    // circuits `process_text_file` (parse_phase.rs) before any chunk is
    // touched, and `spelunk check` (check.rs) only re-hashes file content
    // against the stored hash, never cross-checking chunk presence. Neither
    // sees anything wrong with target.py after the crash in the drill above,
    // so nothing currently converges this file back to indexed without
    // `--force`. This test intentionally fails the moment that gap is closed;
    // update it alongside the fix rather than deleting it.
    let f = crash_mid_target_file();

    let mut cmd = spelunk_command(f._home.path());
    let out = cmd
        .current_dir(f.project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("run plain re-index");
    assert!(
        out.status.success(),
        "a plain re-index must not itself fail: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let conn = Connection::open(&f.db_path).expect("open db");
    assert_eq!(
        chunk_count_for(&conn, "target.py"),
        0,
        "documents the current gap: a plain re-index skips target.py forever because its hash \
         is already current, even though it has zero chunks"
    );
}

#[test]
fn force_reindex_heals_the_interrupted_file() {
    let f = crash_mid_target_file();

    let mut cmd = spelunk_command(f._home.path());
    let out = cmd
        .current_dir(f.project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .arg("--force")
        .output()
        .expect("run forced re-index");
    assert!(
        out.status.success(),
        "forced re-index must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_integrity_ok(&f.db_path);
    let conn = Connection::open(&f.db_path).expect("open db");
    assert!(
        chunk_count_for(&conn, "target.py") > 0,
        "--force bypasses the hash-skip check, so it must recover the interrupted file"
    );
    for path in ["alpha.py", "gamma.py", "target.py"] {
        assert!(
            all_file_paths(&conn).contains(&path.to_string()),
            "{path} must be present after a full forced re-index"
        );
    }
}

// ── Drill 4: the embed-phase crash window ────────────────────────────────────
//
// Unlike the parse-write path above, `insert_embeddings` (db.rs) commits one
// whole batch per transaction by design (ADR-070 D2), and
// `chunks_missing_embeddings` (chunks.rs) re-derives the embed queue from
// presence/absence of an `embeddings` row rather than trusting any in-memory
// state. This drill exercises that resume path end to end through the real
// CLI orchestration (parse -> embed -> a second, independent process), not
// just the single already-covered unit test that hard-exits mid-transaction
// in-process (`insert_embeddings_shaped_batch_leaves_nothing_after_a_hard_
// process_exit` in spelunk-core).

struct EmbedFixture {
    _home: TempDir,
    project: TempDir,
    db_path: PathBuf,
    server: wiremock::MockServer,
}

fn embed_fixture(rt: &tokio::runtime::Runtime) -> EmbedFixture {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    std::fs::write(project.path().join("one.py"), "def one():\n    return 1\n").unwrap();
    std::fs::write(project.path().join("two.py"), "def two():\n    return 2\n").unwrap();
    let db_path = project.path().join(".spelunk").join("index.db");

    let server = rt.block_on(async {
        let server = wiremock::MockServer::start().await;
        mount_health(&server).await;
        mount_index_embed(&server).await;
        server
    });
    write_project_server_config(project.path(), &server.uri(), "test-org/test-project");

    EmbedFixture {
        _home: home,
        project,
        db_path,
        server,
    }
}

#[test]
fn sigkill_mid_embed_phase_resumes_exactly_the_missing_chunk() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let f = embed_fixture(&rt);

    // 2 chunks total: calibration batch 1 takes exactly 1 (CALIBRATION_BATCH_1),
    // so pausing after "after_embed_batch:1" commits leaves exactly 1 embedded
    // and 1 missing - a small, deterministic split, not an approximation.
    let mut cmd = spelunk_command(f._home.path());
    cmd.current_dir(f.project.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--no-summaries");
    let paused = spawn_paused_at(cmd, "after_embed_batch:1");
    kill_and_reap(paused);

    assert_integrity_ok(&f.db_path);
    {
        register_sqlite_vec();
        let conn = Connection::open(&f.db_path).expect("open db");
        assert_eq!(
            embedding_count(&conn),
            1,
            "exactly the first calibration batch must have committed before the kill"
        );
    }

    // Plain re-run: both files' hashes are already current, so parse_phase
    // skips reparsing them, but the missing-embeddings backfill union must
    // still queue the one chunk that never got embedded.
    let mut cmd2 = spelunk_command(f._home.path());
    let out = cmd2
        .current_dir(f.project.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--no-summaries")
        .output()
        .expect("run resume index");
    assert!(
        out.status.success(),
        "resume run must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    register_sqlite_vec();
    let conn = Connection::open(&f.db_path).expect("reopen db");
    assert_eq!(
        embedding_count(&conn),
        2,
        "the resume run must have embedded exactly the missing chunk, reaching full coverage"
    );
    let distinct: i64 = conn
        .query_row("SELECT COUNT(DISTINCT chunk_id) FROM embeddings", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        distinct, 2,
        "no chunk may have been embedded twice (insert_embeddings uses delete-then-insert per \
         chunk_id, so a duplicate here would mean the resume re-queued an already-embedded chunk)"
    );
    drop(f.server);
}

// ── Drill 5-6: disk-full (SQLITE_FULL) surfaces cleanly, never corrupts ──────
//
// `SPELUNK_TEST_MAX_PAGE_COUNT` caps a freshly-opened connection's
// `PRAGMA max_page_count` (see `storage::apply_test_page_cap`), which forces
// the identical `SQLITE_FULL` SQLite would raise for a real disk-full without
// needing a size-capped filesystem or a custom VFS - `max_page_count` is a
// per-connection setting SQLite does not persist to the file, so a fresh,
// uncapped process re-opening the same file afterward behaves like any real
// disk-full recovery: the earlier writer's cap is gone, not baked into the DB.

#[test]
fn disk_full_during_index_surfaces_a_clean_error_and_db_stays_valid() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    std::fs::write(
        project.path().join("seed.py"),
        "def seed():\n    return 0\n",
    )
    .unwrap();
    let db_path = project.path().join(".spelunk").join("index.db");

    // Uncapped baseline: establishes the schema and a small amount of data,
    // so the capped run below is growing an existing file, not failing
    // during first-open migrations (which would test migration behaviour,
    // not the index write path this drill targets).
    let mut baseline = spelunk_command(home.path());
    let out = baseline
        .current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("baseline index");
    assert!(out.status.success(), "baseline index must succeed");
    let baseline_pages = page_count(&db_path);

    // A lot of new content, forced to fully reparse: guarantees the write
    // volume needed to blow past a cap set just above the baseline, however
    // small the margin.
    for i in 0..40 {
        std::fs::write(
            project.path().join(format!("bulk_{i}.py")),
            format!(
                "def bulk_{i}():\n    \"\"\"{}\n    padding to grow the row.\n    \"\"\"\n    return {i}\n",
                "x".repeat(400)
            ),
        )
        .unwrap();
    }

    let mut capped = spelunk_command(home.path());
    let out = capped
        .current_dir(project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .env(
            "SPELUNK_TEST_MAX_PAGE_COUNT",
            (baseline_pages + 2).to_string(),
        )
        .arg("index")
        .arg(".")
        .arg("--force")
        .output()
        .expect("capped index");

    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        !out.status.success(),
        "a run that cannot fit its writes must not report success"
    );
    assert!(
        !stderr.contains("panicked"),
        "a full disk must surface as a returned error, never a Rust panic: {stderr}"
    );
    assert!(
        stderr.contains("full") || stderr.contains("disk"),
        "the error must name the actual condition (SQLite's own SQLITE_FULL message says \
         'database or disk is full'), not a generic failure: {stderr}"
    );

    // The cap was per-connection: a fresh, uncapped open must succeed and
    // find a structurally valid file.
    assert_integrity_ok(&db_path);
}

#[test]
fn disk_full_during_memory_add_surfaces_a_clean_error_and_note_is_not_partially_stored() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let mem_db = project.path().join(".spelunk").join("memory.db");
    let config_path = project.path().join("config.toml");
    std::fs::write(
        &config_path,
        "llm_model = \"test-model\"\nstore_in_git_notes = false\n",
    )
    .unwrap();

    let memory_add =
        |home: &Path, extra_env: Option<(&str, &str)>, body: &str| -> std::process::Output {
            let mut cmd = spelunk_command(home);
            cmd.current_dir(project.path())
                .env("SPELUNK_NO_SERVER", "1")
                .env_remove("SPELUNK_SERVER_URL")
                .arg("--config")
                .arg(&config_path)
                .arg("memory")
                .arg("--db")
                .arg(&mem_db)
                .arg("add")
                .arg("--kind")
                .arg("note")
                .arg("--title")
                .arg("baseline")
                .arg("--body")
                .arg(body);
            if let Some((k, v)) = extra_env {
                cmd.env(k, v);
            }
            cmd.output().expect("run memory add")
        };

    let out = memory_add(home.path(), None, "seed note");
    assert!(
        out.status.success(),
        "baseline memory add must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let baseline_pages = page_count(&mem_db);
    let baseline_rows: i64 = {
        let conn = Connection::open(&mem_db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap()
    };

    // Large enough to need far more than the +2-page cap margin below, small
    // enough to stay under the OS argv-length limit (`ArgumentListTooLong`
    // above ~256KB-1MB depending on platform).
    let huge_body = "y".repeat(150_000);
    let out = memory_add(
        home.path(),
        Some((
            "SPELUNK_TEST_MAX_PAGE_COUNT",
            &(baseline_pages + 2).to_string(),
        )),
        &huge_body,
    );

    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        !out.status.success(),
        "an add that cannot fit must not report success"
    );
    assert!(
        !stderr.contains("panicked"),
        "must surface as a returned error, never a panic: {stderr}"
    );
    assert!(
        stderr.contains("full") || stderr.contains("disk"),
        "error must name the actual condition: {stderr}"
    );

    assert_integrity_ok(&mem_db);
    let conn = Connection::open(&mem_db).unwrap();
    let rows_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        rows_after, baseline_rows,
        "a single failed INSERT must leave no partial row: SQLite's own per-statement \
         autocommit already guarantees this (unlike the multi-statement index write path above), \
         so this is the positive control confirming the cap actually exercised SQLITE_FULL \
         rather than silently no-op'ing"
    );
}

// ── Drill 7: two concurrent `spelunk index` runs on one project ─────────────
//
// Neither index.db, memory.db, nor registry.db ever sets `PRAGMA
// busy_timeout` anywhere in this codebase (confirmed by reading `Database::
// open`, `MemoryStore::open`, and `Registry::init`), so a second writer that
// arrives while another holds the WAL write lock gets `SQLITE_BUSY`
// immediately, not after a retry window.
//
// CONFIRMED FINDING, not a hypothetical: this drill reproducibly hits real
// SQLite-level corruption ("database disk image is malformed", SQLITE_CORRUPT)
// on unmodified code, not merely a busy/locked error from the losing process.
// A single trial only overlaps the two processes' writes probabilistically
// (observed ~1-in-3 to ~1-in-2 across manual sampling), so this loops several
// fresh trials to make the drill a reliable regression pin instead of a flaky
// one - this suite intentionally ships this test RED against unfixed main;
// see the story's final report for the severity call and recommended next
// step (a cross-process lock around the whole index run, the same shape as
// `storage::git_notes::lock`, since WAL concurrent-writer semantics alone do
// not prevent this).
//
// Marked `#[ignore]` deliberately, not deleted or softened: the corruption is
// real but probabilistic even across `TRIALS` runs (observed ~1-in-3 outer
// invocations reproducing it locally), so it cannot sit in the default green
// suite without either being flaky-red (blocks merges on bad luck) or
// training reviewers to ignore a real signal. Run explicitly with
// `cargo test -p spelunk-cli --test crash_safety -- --ignored
// two_concurrent_index_runs_on_one_project_do_not_corrupt_the_db` to
// reproduce, and un-ignore once the cross-process index lock lands - this
// test is the fix's regression test, written in advance.
#[test]
#[ignore = "pins a confirmed, reproducible SQLITE_CORRUPT from two concurrent `spelunk index` \
            runs (see module doc comment); un-ignore once a cross-process index lock lands"]
fn two_concurrent_index_runs_on_one_project_do_not_corrupt_the_db() {
    const TRIALS: usize = 8;
    const FILES_PER_TRIAL: usize = 150;

    for trial in 0..TRIALS {
        let home = TempDir::new().expect("home");
        let project = TempDir::new().expect("project");
        for i in 0..FILES_PER_TRIAL {
            std::fs::write(
                project.path().join(format!("f{i}.py")),
                format!("def f{i}():\n    return {i}\n"),
            )
            .unwrap();
        }
        let db_path = project.path().join(".spelunk").join("index.db");

        let run = |home_dir: PathBuf, project_dir: PathBuf| {
            std::thread::spawn(move || {
                let mut cmd = spelunk_command(&home_dir);
                cmd.current_dir(&project_dir)
                    .env("SPELUNK_NO_SERVER", "1")
                    .arg("index")
                    .arg(".")
                    .arg("--force")
                    .arg("--no-summaries")
                    .output()
                    .expect("run concurrent index")
            })
        };

        let t1 = run(home.path().to_path_buf(), project.path().to_path_buf());
        let t2 = run(home.path().to_path_buf(), project.path().to_path_buf());
        let out1 = t1.join().expect("thread 1");
        let out2 = t2.join().expect("thread 2");

        for (label, out) in [("run 1", &out1), ("run 2", &out2)] {
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                !stderr.to_lowercase().contains("panicked"),
                "trial {trial}, {label} must never panic, whichever loses the race: {stderr}"
            );
        }

        // The decisive assertion: whichever process won (or both, if their
        // writes never actually overlapped this trial), the file must not be
        // corrupted. This is expected to fail within the trial budget above
        // on unmodified code - see the doc comment on this test.
        let conn = Connection::open(&db_path).expect("reopen db after concurrent runs");
        let result: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .expect("run integrity_check");
        assert_eq!(
            result, "ok",
            "trial {trial}/{TRIALS}: two concurrent `spelunk index --force` runs on the same \
             project corrupted index.db (SQLite integrity_check: {result:?}). This is real, \
             reachable data loss - not a benign SQLITE_BUSY from the losing process - and is not \
             fixed by adding `busy_timeout` alone (that only changes how long a writer waits for \
             the lock, not what happens if both still interleave statement-by-statement); it \
             needs a cross-process lock around the whole index run, see the module doc comment"
        );
    }
}

// ── Drill 8: a concurrent reader is never blocked by an open writer ─────────
//
// WAL mode's whole purpose is that a reader never contends with a writer -
// only writer-vs-writer does. This pins that guarantee for the real CLI
// paths: `spelunk search --mode text` (a pure FTS read against index.db)
// must complete cleanly while a `spelunk index` embed batch's transaction is
// genuinely open (held via `storage::pause_for_crash_test("embed_tx_open")`,
// not merely "probably in progress").

#[test]
fn concurrent_full_text_search_during_an_open_embed_transaction_never_sees_busy() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let f = embed_fixture(&rt);

    let mut cmd = spelunk_command(f._home.path());
    cmd.current_dir(f.project.path())
        .env("SPELUNK_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--no-summaries");
    let paused = spawn_paused_at(cmd, "embed_tx_open");

    let mut search_cmd = spelunk_command(f._home.path());
    let search_out = search_cmd
        .current_dir(f.project.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("search")
        .arg("one")
        .arg("--mode")
        .arg("text")
        .arg("--db")
        .arg(&f.db_path)
        .arg("--no-stale-check")
        .output()
        .expect("run concurrent search");

    let search_stderr = String::from_utf8_lossy(&search_out.stderr).to_lowercase();
    assert!(
        search_out.status.success(),
        "a concurrent read must succeed while a writer transaction is open (WAL mode): {}",
        search_stderr
    );
    assert!(
        !search_stderr.contains("busy") && !search_stderr.contains("locked"),
        "a concurrent read must never surface SQLITE_BUSY to the user: {search_stderr}"
    );

    let status = release_and_wait(paused);
    assert!(status.success(), "the released indexer must finish cleanly");
    drop(f.server);
}
