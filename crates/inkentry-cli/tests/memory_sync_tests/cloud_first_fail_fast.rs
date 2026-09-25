// `cloud_first` against an absent server must fail in about one connect timeout, not a
// whole request budget, and name the server as unreachable rather than show a raw
// transport error.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// Several subsystems reach the same server (probe, embed, dialect probe, request); the
// first failure is memoised so the command costs about one connect timeout. 5s tolerates a
// loaded machine, but a lost memo (~9s) or lost connect bound (tens of seconds) still fails.
const FAIL_FAST_CEILING: Duration = Duration::from_secs(5);

// TEST-NET-1 is never routed, so a connect gets no answer rather than a refusal: without a
// connect bound nothing ends the attempt. Must be https: plaintext http to a non-loopback
// host is refused by the transport guard.
const UNROUTABLE_SERVER: &str = "https://192.0.2.1:4655";

const LOCAL_TITLE: &str = "seeded local entry";
const ATTEMPTED_TITLE: &str = "entry the server never accepted";

fn write_cfg(dir: &Path, name: &str, db_path: &Path, extra: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\n\
         llm_model = \"test-chat\"\n{extra}",
        db_path
    );
    let path = dir.join(name);
    std::fs::write(&path, cfg).expect("write config");
    path
}

// Binds an ephemeral port then drops the listener, so connecting is refused outright.
fn closed_loopback_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("read the bound port").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

// Seeds one local entry so the later local read proves the store is readable, which makes
// the absence of a failed write meaningful.
struct Project {
    tmp: TempDir,
    mem_path: PathBuf,
    cfg: PathBuf,
}

fn cloud_first_project(server_url: &str) -> Project {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let mem_path = db_path.with_file_name("memory.db");

    // Not a git repo, so the git-notes write-through is a no-op and the entry lands only in memory.db.
    let seed_cfg = write_cfg(tmp.path(), "config-seed.toml", &db_path, "");
    let out = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&seed_cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args([
            "add",
            "--kind",
            "note",
            "--title",
            LOCAL_TITLE,
            "--body",
            "b",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "seeding the local store must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `mode` is not a project-config field, so it stays in the global file; `server_url`/
    // `project_id` only apply from `.inkentry/config.toml`. A raw-UUID project_id skips slug
    // resolution, so the failure under test is the memory request itself.
    let cfg = write_cfg(
        tmp.path(),
        "config-cloud-first.toml",
        &db_path,
        "mode = \"cloud_first\"\n",
    );
    plumbing_helpers::write_project_server_config(
        tmp.path(),
        server_url,
        "11111111-1111-1111-1111-111111111111",
    );

    Project { tmp, mem_path, cfg }
}

impl Project {
    fn memory(&self, args: &[&str]) -> (std::process::Output, Duration) {
        let started = Instant::now();
        let out = inkentry_bin()
            .current_dir(self.tmp.path())
            .arg("--config")
            .arg(&self.cfg)
            .args(["memory", "--db"])
            .arg(&self.mem_path)
            .args(args)
            .output()
            .unwrap();
        (out, started.elapsed())
    }

    fn local_titles(&self) -> String {
        let elsewhere = TempDir::new().unwrap();
        let cfg = write_cfg(
            elsewhere.path(),
            "config-local-read.toml",
            &self.tmp.path().join("inkentry.db"),
            "",
        );
        let out = inkentry_bin()
            .env("INKENTRY_NO_SERVER", "1")
            .current_dir(elsewhere.path())
            .arg("--config")
            .arg(&cfg)
            .args(["memory", "--db"])
            .arg(&self.mem_path)
            .args(["list", "--format", "json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "the local store must still be readable: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

fn assert_names_the_server_unreachable(stderr: &str, server_url: &str) {
    assert!(
        stderr.contains(&format!("team server unreachable at {server_url}")),
        "the error must name the server it could not reach: {stderr}"
    );
    assert!(
        stderr.contains("cloud_first"),
        "the error must name the mode that produced it: {stderr}"
    );
    assert!(
        stderr.contains("does not fall back to the local store"),
        "the error must say the local store is not a fallback: {stderr}"
    );
}

fn assert_fails_fast(elapsed: Duration, label: &str) {
    assert!(
        elapsed < FAIL_FAST_CEILING,
        "{label} must fail in about a connection attempt, took {elapsed:?}"
    );
}

#[test]
fn cloud_first_write_to_an_unroutable_server_fails_fast() {
    let project = cloud_first_project(UNROUTABLE_SERVER);
    let (out, elapsed) = project.memory(&[
        "add",
        "--kind",
        "note",
        "--title",
        ATTEMPTED_TITLE,
        "--body",
        "b",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "the write must fail: {stderr}");
    assert_fails_fast(elapsed, "a write to an unroutable server");
    assert_names_the_server_unreachable(&stderr, UNROUTABLE_SERVER);
}

#[test]
fn cloud_first_read_from_an_unroutable_server_fails_fast() {
    let project = cloud_first_project(UNROUTABLE_SERVER);
    let (out, elapsed) = project.memory(&["list", "--format", "json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "the read must fail: {stdout}");
    assert_fails_fast(elapsed, "a read from an unroutable server");
    assert_names_the_server_unreachable(&stderr, UNROUTABLE_SERVER);
    assert!(
        !stdout.contains(LOCAL_TITLE),
        "local data must never be served when reads route to the server: {stdout}"
    );
}

#[test]
fn cloud_first_write_to_a_closed_port_is_refused_and_stores_nothing_locally() {
    let server_url = closed_loopback_url();
    let project = cloud_first_project(&server_url);
    let (out, elapsed) = project.memory(&[
        "add",
        "--kind",
        "note",
        "--title",
        ATTEMPTED_TITLE,
        "--body",
        "b",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "the write must fail: {stderr}");
    assert_fails_fast(elapsed, "a write to a closed port");
    assert_names_the_server_unreachable(&stderr, &server_url);

    let titles = project.local_titles();
    assert!(
        titles.contains(LOCAL_TITLE),
        "the local store must still be readable and hold what was seeded: {titles}"
    );
    assert!(
        !titles.contains(ATTEMPTED_TITLE),
        "a failed cloud_first write must not land in the local store: {titles}"
    );
}

// Accepts TCP and holds connections open without speaking: the connect succeeds and the TLS
// handshake never completes, so only a bound covering the handshake ends it. Stand-in for a
// dropped SYN, and the case a loopback exemption would reintroduce.
fn spawn_stalling_loopback_listener() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind stall listener");
    let port = listener.local_addr().expect("local_addr").port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }
    });
    port
}

#[test]
fn cloud_first_write_to_a_stalled_loopback_server_is_still_bounded() {
    let port = spawn_stalling_loopback_listener();
    let project = cloud_first_project(&format!("https://127.0.0.1:{port}"));
    let (out, elapsed) = project.memory(&[
        "add",
        "--kind",
        "note",
        "--title",
        ATTEMPTED_TITLE,
        "--body",
        "b",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "the write must fail: {stderr}");
    assert_fails_fast(elapsed, "a write to a stalled loopback server");
    assert!(
        !project.local_titles().contains(ATTEMPTED_TITLE),
        "a failed cloud_first write must not land in the local store"
    );
}
