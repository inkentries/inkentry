// Drills target windows this codebase controls (transaction boundaries, hash resume/skip,
// index.db/memory.db consistency, concurrent access), not SQLite's own durability. Every
// SIGKILL is real: a real `inkentry` child parked at a write window by the env-gated crash
// hook and killed with `Child::kill()`; nothing simulates a crash in-process.

use crate::plumbing_helpers;

use plumbing_helpers::{
    mount_health, mount_index_embed, register_sqlite_vec, write_project_server_config,
};
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

const MARKER_TIMEOUT: Duration = Duration::from_secs(30);

// A raw `std::process::Command` rather than `assert_cmd`'s, which cannot expose the stdio
// control the marker-then-kill protocol needs.
fn inkentry_command(home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("inkentry"));
    cmd.env("INKENTRY_SECRET_STORE", "file")
        .env("INKENTRY_TEST_DISCOVERY_PORT", "0")
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_CONFIG_DIR", home.join(".config").join("inkentry"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

// Stdout is drained into `stdout_so_far` so the child never blocks on a full pipe and a failed
// assertion can print what it said.
struct PausedChild {
    child: Child,
    stdout_so_far: std::sync::Arc<std::sync::Mutex<String>>,
}

// Blocks until the child prints the REACHED marker, proving it is parked in the window under
// test rather than "probably there by now"; panics with the child's stdout if it never does.
fn spawn_paused_at(mut cmd: Command, point: &str) -> PausedChild {
    cmd.env("INKENTRY_TEST_CRASH_POINT", point)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn inkentry");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Drain stderr too so a chatty child can't deadlock on a full pipe.
    std::thread::spawn(move || {
        let mut r = BufReader::new(stderr);
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });

    let buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let buf_writer = buf.clone();
    let marker = format!("INKENTRY_TEST_CRASH_POINT_REACHED:{point}");
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
                        // Keep draining so the child never blocks on a full stdout pipe.
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

// Asserts death by signal: a coincidental clean exit would mean the drill never tested a crash.
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

// For drills that need a held write/lock window without testing a kill.
fn release_and_wait(mut pc: PausedChild) -> std::process::ExitStatus {
    {
        let stdin = pc.child.stdin.as_mut().expect("piped stdin");
        let _ = stdin.write_all(b"\n");
    }
    pc.child.wait().expect("wait for released child")
}

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

// `process_text_file` commits the file's new hash via `upsert_file` before deleting and
// inserting its chunks, with no transaction spanning the two: a SIGKILL between them leaves
// `files.hash` current while `chunks` is empty.

struct InterruptedFixture {
    _home: TempDir,
    project: TempDir,
    db_path: PathBuf,
}

// The crash point targets `target.py` so the window is pinned regardless of walk order; the
// other two files are only asserted fully present or fully absent.
fn write_three_file_project(dir: &Path) {
    std::fs::write(dir.join("alpha.py"), "def alpha():\n    return 1\n").unwrap();
    std::fs::write(dir.join("target.py"), "def target():\n    return 2\n").unwrap();
    std::fs::write(dir.join("gamma.py"), "def gamma():\n    return 3\n").unwrap();
}

fn crash_mid_target_file() -> InterruptedFixture {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    write_three_file_project(project.path());
    let db_path = project.path().join(".inkentry").join("index.db");

    let mut cmd = inkentry_command(home.path());
    cmd.current_dir(project.path())
        .env("INKENTRY_NO_SERVER", "1")
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

    // The other files must be fully present or fully absent, never target.py's half-state;
    // walk order is not pinned.
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
fn plain_reindex_heals_a_hash_current_empty_chunks_file() {
    let f = crash_mid_target_file();

    let mut cmd = inkentry_command(f._home.path());
    let out = cmd
        .current_dir(f.project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("run plain re-index");
    assert!(
        out.status.success(),
        "a plain re-index must not itself fail: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_integrity_ok(&f.db_path);
    let conn = Connection::open(&f.db_path).expect("open db");
    assert!(
        chunk_count_for(&conn, "target.py") > 0,
        "a plain re-index (no --force) must self-heal a hash-current, zero-chunk file left \
         behind by the interrupted crash window"
    );
    for path in ["alpha.py", "gamma.py", "target.py"] {
        assert!(
            all_file_paths(&conn).contains(&path.to_string()),
            "{path} must be present after the plain re-index"
        );
    }
}

#[test]
fn plain_reindex_keeps_reprocessing_a_legitimately_empty_file_every_run() {
    // An empty file parses to zero chunks by design and `file_has_chunks` cannot tell that from
    // the crash-window state, so it is reprocessed on every plain re-index: accepted extra
    // work, not a correctness issue.
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    std::fs::write(
        project.path().join("normal.py"),
        "def normal():\n    return 1\n",
    )
    .unwrap();
    std::fs::write(project.path().join("empty.py"), "").unwrap();
    let db_path = project.path().join(".inkentry").join("index.db");

    let mut first = inkentry_command(home.path());
    let first_out = first
        .current_dir(project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("first index");
    assert!(
        first_out.status.success(),
        "first index must succeed: {}",
        String::from_utf8_lossy(&first_out.stderr)
    );

    {
        let conn = Connection::open(&db_path).expect("open db");
        assert!(
            file_hash(&conn, "empty.py").is_some(),
            "an empty file is still indexed (present in `files`) with a real content hash"
        );
        assert_eq!(
            chunk_count_for(&conn, "empty.py"),
            0,
            "an empty file legitimately produces zero chunks - not a crash artifact"
        );
    }

    // The decisive check: a second re-index must still reach empty.py's per-file processing
    // (the pause point fires); if it were skipped, `spawn_paused_at` would time out.
    let mut second = inkentry_command(home.path());
    second
        .current_dir(project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused = spawn_paused_at(second, "after_index_hash_write:empty.py");
    let status = release_and_wait(paused);
    assert!(
        status.success(),
        "the released second re-index must finish cleanly"
    );
}

#[test]
fn force_reindex_heals_the_interrupted_file() {
    let f = crash_mid_target_file();

    let mut cmd = inkentry_command(f._home.path());
    let out = cmd
        .current_dir(f.project.path())
        .env("INKENTRY_NO_SERVER", "1")
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

// `insert_embeddings` commits one whole batch per transaction and
// `chunks_missing_embeddings` re-derives the embed queue from the absence of an `embeddings`
// row; this exercises that resume path through the real CLI across two processes.
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
    let db_path = project.path().join(".inkentry").join("index.db");

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

    // 2 chunks, and calibration batch 1 takes exactly 1, so pausing after
    // "after_embed_batch:1" leaves exactly 1 embedded and 1 missing.
    let mut cmd = inkentry_command(f._home.path());
    cmd.current_dir(f.project.path())
        .env("INKENTRY_MODE", "cloud_first")
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

    // Both files' hashes are current so parsing skips them, but the missing-embeddings
    // backfill must still queue the one chunk that never got embedded.
    let mut cmd2 = inkentry_command(f._home.path());
    let out = cmd2
        .current_dir(f.project.path())
        .env("INKENTRY_MODE", "cloud_first")
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

// `INKENTRY_TEST_MAX_PAGE_COUNT` caps a fresh connection's `max_page_count`, forcing the
// SQLITE_FULL a real disk-full raises without a size-capped filesystem. The cap is
// per-connection and not persisted, so a fresh uncapped process re-opening the file behaves
// like real disk-full recovery.
#[test]
fn disk_full_during_index_surfaces_a_clean_error_and_db_stays_valid() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    std::fs::write(
        project.path().join("seed.py"),
        "def seed():\n    return 0\n",
    )
    .unwrap();
    let db_path = project.path().join(".inkentry").join("index.db");

    // Uncapped baseline so the capped run grows an existing file rather than failing in
    // first-open migrations.
    let mut baseline = inkentry_command(home.path());
    let out = baseline
        .current_dir(project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("baseline index");
    assert!(out.status.success(), "baseline index must succeed");
    let baseline_pages = page_count(&db_path);

    // Enough new content, force-reparsed, to blow past a cap set just above the baseline.
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

    let mut capped = inkentry_command(home.path());
    let out = capped
        .current_dir(project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env(
            "INKENTRY_TEST_MAX_PAGE_COUNT",
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

    assert_integrity_ok(&db_path);
}

#[test]
fn disk_full_during_memory_add_surfaces_a_clean_error_and_note_is_not_partially_stored() {
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    let mem_db = project.path().join(".inkentry").join("memory.db");
    let config_path = project.path().join("config.toml");
    std::fs::write(
        &config_path,
        "llm_model = \"test-model\"\nstore_in_git_notes = false\n",
    )
    .unwrap();

    let memory_add =
        |home: &Path, extra_env: Option<(&str, &str)>, body: &str| -> std::process::Output {
            let mut cmd = inkentry_command(home);
            cmd.current_dir(project.path())
                .env("INKENTRY_NO_SERVER", "1")
                .env_remove("INKENTRY_SERVER_URL")
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

    // Far beyond the +2-page cap margin (FTS5 roughly doubles stored bytes) yet under argv
    // limits: Windows caps the command line near 32KB, Linux a single argv entry at 128KB.
    let huge_body = "y".repeat(20_000);
    let out = memory_add(
        home.path(),
        Some((
            "INKENTRY_TEST_MAX_PAGE_COUNT",
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

// Neither db sets `busy_timeout`, so a second writer gets SQLITE_BUSY immediately. `run_lock.rs`'s
// non-blocking per-project advisory lock makes a second `index` exit with a clean "already
// running" error before touching the DB.
#[test]
fn two_concurrent_index_runs_on_one_project_do_not_corrupt_the_db() {
    const TRIALS: usize = 8;
    const FILES_PER_TRIAL: usize = 150;

    // At least one trial must actually contend for the lock, else back-to-back runs pass
    // trivially.
    let mut observed_contention = false;

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
        let db_path = project.path().join(".inkentry").join("index.db");

        let run = |home_dir: PathBuf, project_dir: PathBuf| {
            std::thread::spawn(move || {
                let mut cmd = inkentry_command(&home_dir);
                cmd.current_dir(&project_dir)
                    .env("INKENTRY_NO_SERVER", "1")
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

        let successes = [&out1, &out2].iter().filter(|o| o.status.success()).count();
        assert!(
            successes >= 1,
            "trial {trial}: at least one of the two concurrent runs must complete successfully: \
             run1={:?} run2={:?}",
            out1.status,
            out2.status
        );

        for (label, out) in [("run 1", &out1), ("run 2", &out2)] {
            if !out.status.success() {
                observed_contention = true;
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(
                    stderr.contains("already running"),
                    "trial {trial}, {label}: a losing process must fail with the clean \
                     lock-contention error, not some other failure: {stderr}"
                );
            }
        }

        assert_integrity_ok(&db_path);

        let conn = Connection::open(&db_path).expect("reopen db after concurrent runs");
        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .expect("count files");
        assert_eq!(
            file_count, FILES_PER_TRIAL as i64,
            "trial {trial}: the project must be fully indexed by whichever process(es) \
             actually wrote, with no half-written state from an aborted loser"
        );
    }

    assert!(
        observed_contention,
        "across {TRIALS} trials, the two concurrent runs never actually contended for the lock \
         (both always happened to run fully sequentially) - this test never exercised the \
         behaviour it is meant to pin"
    );
}

// WAL readers never contend with a writer: a pure FTS `search` must complete while an `index`
// embed batch's transaction is genuinely open (held via `embed_tx_open`).
#[test]
fn concurrent_full_text_search_during_an_open_embed_transaction_never_sees_busy() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let f = embed_fixture(&rt);

    let mut cmd = inkentry_command(f._home.path());
    cmd.current_dir(f.project.path())
        .env("INKENTRY_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--no-summaries");
    let paused = spawn_paused_at(cmd, "embed_tx_open");

    let mut search_cmd = inkentry_command(f._home.path());
    let search_out = search_cmd
        .current_dir(f.project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("search")
        .arg("one")
        .arg("--only-text")
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

#[test]
fn sigkilled_lock_holder_never_wedges_a_future_index_run() {
    // The kill lands well before either continuation-spawn site releases the lock, so its fd
    // closes only because the OS reaped the process.
    let f = crash_mid_target_file();

    let mut cmd = inkentry_command(f._home.path());
    let out = cmd
        .current_dir(f.project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("run index after the lock holder was killed");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a fresh index run after the lock holder was SIGKILLed must succeed, not report the \
         lock as still held: {stderr}"
    );
    assert!(
        !stderr.contains("already running"),
        "the OS advisory lock must be released when the holder's process dies by SIGKILL - \
         there is no stale-lock cleanup path in run_lock.rs, and a killed holder must not \
         need one: {stderr}"
    );
    assert_integrity_ok(&f.db_path);
}

#[test]
fn concurrent_index_on_different_projects_is_not_blocked_by_an_unrelated_lock() {
    // A lock keyed broader than the project would hang or fail project B while A's run is
    // merely in progress.
    let home = TempDir::new().expect("home");

    let project_a = TempDir::new().expect("project a");
    write_three_file_project(project_a.path());
    let mut cmd_a = inkentry_command(home.path());
    cmd_a
        .current_dir(project_a.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused_a = spawn_paused_at(cmd_a, "after_index_hash_write:target.py");

    let project_b = TempDir::new().expect("project b");
    write_three_file_project(project_b.path());
    let mut cmd_b = inkentry_command(home.path());
    let out_b = cmd_b
        .current_dir(project_b.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".")
        .output()
        .expect("run index on project b while project a's run is held open");

    assert!(
        out_b.status.success(),
        "indexing project B must not be blocked by project A's held lock: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );

    kill_and_reap(paused_a);
}

#[test]
fn losing_child_continuation_mode_fails_clean_without_touching_the_db() {
    // A continuation child losing its lock re-acquisition to another holder (held
    // deterministically here) must bail before `Database::open`, never interleave writes:
    // `index()` re-acquires the per-project lock before either `--_background-phases` or
    // `--_embed-phases` does real work.
    let home = TempDir::new().expect("home");
    let project = TempDir::new().expect("project");
    write_three_file_project(project.path());
    let db_path = project.path().join(".inkentry").join("index.db");

    let mut cmd = inkentry_command(home.path());
    cmd.current_dir(project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused = spawn_paused_at(cmd, "after_index_hash_write:target.py");

    let page_count_while_held = page_count(&db_path);

    for mode_flag in ["--_background-phases", "--_embed-phases"] {
        let mut child_cmd = inkentry_command(home.path());
        let child_out = child_cmd
            .current_dir(project.path())
            .env("INKENTRY_NO_SERVER", "1")
            .arg("index")
            .arg(".")
            .arg(mode_flag)
            .output()
            .expect("run continuation-mode child while the lock is held");

        assert!(
            !child_out.status.success(),
            "a {mode_flag} child must fail while the project's lock is held by another \
             process, not proceed"
        );
        let child_stderr = String::from_utf8_lossy(&child_out.stderr);
        assert!(
            child_stderr.contains("already running"),
            "{mode_flag} must fail with the clean lock-contention error, not some other \
             failure: {child_stderr}"
        );
        assert_eq!(
            page_count(&db_path),
            page_count_while_held,
            "a {mode_flag} child that loses the lock race must never touch the db - not even \
             open it - so the page count must be identical before and after the attempt"
        );
    }

    assert_integrity_ok(&db_path);
    kill_and_reap(paused);
}

#[test]
fn parent_reports_the_handoff_honestly_when_a_third_process_wins_the_lock_race() {
    // Unlike the test above, this drives the real parent-releases/parent-spawns handoff: the
    // parent must not claim "embedding in the background" unless the spawned child became the
    // lock's recorded holder (`wait_for_holder_pid`). Reproduced deterministically: pause the
    // parent after it releases the lock, let a separate `index` process win it, then resume.
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let f = embed_fixture(&rt);

    let mut cmd = inkentry_command(f._home.path());
    cmd.current_dir(f.project.path())
        .env("INKENTRY_MODE", "cloud_first")
        .arg("index")
        .arg(".")
        .arg("--detach-embed")
        .arg("--no-summaries");
    let paused_parent = spawn_paused_at(cmd, "after_run_lock_drop:embed");
    let parent_stdout = paused_parent.stdout_so_far.clone();

    // The parent's parse phase already hashed one.py/two.py, so a third run over them would
    // skip past the hash-write pause point; a new file gives it something to hash.
    std::fs::write(
        f.project.path().join("three.py"),
        "def three():\n    return 3\n",
    )
    .expect("write third file");

    // The parent has released the lock but not yet spawned its child; a separate process
    // wins it here and holds it past the child's bounded confirmation window.
    let mut third_cmd = inkentry_command(f._home.path());
    third_cmd
        .current_dir(f.project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg(".");
    let paused_third = spawn_paused_at(third_cmd, "after_index_hash_write:three.py");

    let status = release_and_wait(paused_parent);
    assert!(
        status.success(),
        "the parent must still exit cleanly even though its handoff was raced away"
    );

    let stdout = parent_stdout.lock().unwrap().clone();
    assert!(
        !stdout.contains("in the background"),
        "must not claim embedding is proceeding in the background when the spawned child never \
         confirmed it took over the lock: {stdout}"
    );
    assert!(
        stdout.contains("claimed this project's lock"),
        "must tell the user why the background handoff could not be confirmed: {stdout}"
    );
    assert!(
        stdout.contains("Run `inkentry index` again"),
        "must give the user a concrete recovery step rather than leaving the chunks silently \
         unembedded forever: {stdout}"
    );

    kill_and_reap(paused_third);
    assert_integrity_ok(&f.db_path);
}
