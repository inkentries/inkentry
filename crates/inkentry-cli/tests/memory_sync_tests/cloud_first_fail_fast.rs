// `cloud_first` against a server that is not there.
//
// The guarantee under test elsewhere in this directory is that reads and
// writes fail loudly rather than falling back to the local store. These tests
// pin what that failure costs and what it says: it arrives in about the time a
// connection attempt is allowed to take, not the time a whole request is
// allowed to take, and it names the server as unreachable rather than handing
// the reader a raw transport error under a URL.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// What is bounded is a single connection attempt, at 2s. A command makes a few
// of them in sequence, each from a different subsystem that independently finds
// the server absent: the capability probe, the embed of the new entry, the
// dialect probe, then the request itself. A write against an unroutable host
// measures about 9s locally and a read about 5s.
//
// This ceiling is therefore loose on purpose. It is set to catch the failure it
// exists for, an attempt that is not bounded at all and so runs to a request
// budget or to the operating system's own connect budget, which measured 81s
// and 33s for a write and a read before this was fixed. Tightening it to the
// observed figures would buy nothing and would go flaky on a loaded machine.
const FAIL_FAST_CEILING: Duration = Duration::from_secs(20);

// TEST-NET-1, reserved for documentation and guaranteed not to be routed, so a
// connection attempt gets no answer at all rather than a refusal. That is the
// shape of the failure this fix exists for: without a connect bound there is
// nothing for the attempt to fail on until the request budget runs out.
// Plaintext http to a non-loopback host is refused by the transport guard, so
// the scheme has to be https.
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

// A loopback address with nothing listening: bind an ephemeral port, read it
// back, then drop the listener. Connecting there is refused outright, which is
// the other half of "the server is not there" and must read the same way.
fn closed_loopback_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("read the bound port").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

// A project whose local memory.db holds one entry, plus a `cloud_first` config
// pointing at `server_url`. The seeded entry exists so a later local read
// proves the store is present and readable, which is what makes the absence of
// a failed write meaningful rather than vacuous.
struct Project {
    tmp: TempDir,
    mem_path: PathBuf,
    cfg: PathBuf,
}

fn cloud_first_project(server_url: &str) -> Project {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let mem_path = db_path.with_file_name("memory.db");

    // Seeded with no server configured, so this write takes the plain local
    // path. Not a git repo, so the git-notes write-through is a no-op and the
    // entry lands only in memory.db.
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

    // `mode` is not a project-config field, so it stays in the global file,
    // while `server_url`/`project_id` only take effect from the project-level
    // `.inkentry/config.toml`. A raw-UUID project_id skips slug resolution, so
    // the failure under test is the memory request itself.
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

    // Read the local store directly, with no project config in scope and no
    // server contact, to see what the failed write did or did not leave behind.
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

// ── an unroutable server: no answer at all, so only a connect bound ends it ───

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

// ── a closed port: refused outright, and the write leaves nothing behind ──────

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

    // The guarantee this mode is built on: a write the server never accepted is
    // not quietly kept locally either.
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
