// Real-process coverage for the startup guard that refuses to send a
// configured LLM credential over a plaintext non-loopback hop.
//
// The unit tests in `server_llm.rs` pin the decision itself; these prove the
// compiled binary a user actually runs enforces it, with a non-zero exit and
// an error naming the endpoint.
//
// The guard runs before the DB is opened, so each case points `--db` at a
// path inside a directory that does not exist. Every invocation therefore
// fails, and it is the stderr that says which check fired: the transport
// error means the guard tripped, the db error means the guard let the
// configuration through. Nothing binds a socket or warms the embedder, so
// these stay fast and offline.

use std::process::{Command, Output};

fn start_with(llm_url: &str, key: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spelunk-server"));
    cmd.args([
        "--host",
        "127.0.0.1",
        "--port",
        "7777",
        "--db",
        "/nonexistent-spelunk-test-dir/server.db",
        "--llm-url",
        llm_url,
    ]);
    match key {
        Some(k) => cmd.env("SPELUNK_LLM_KEY", k),
        None => cmd.env_remove("SPELUNK_LLM_KEY"),
    };
    cmd.output().expect("spawning spelunk-server")
}

fn assert_reached_the_db(out: &Output, case: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("server db"),
        "{case}: startup should have got past the transport guard to the db open, \
         but stderr was: {stderr}"
    );
}

#[test]
fn a_key_over_plaintext_to_a_non_loopback_host_refuses_to_start() {
    let out = start_with("http://192.168.1.10:1234", Some("sk-llm-secret"));

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("192.168.1.10"),
        "the error must name the offending endpoint: {stderr}"
    );
    assert!(
        !stderr.contains("sk-llm-secret"),
        "the credential must never appear in output: {stderr}"
    );
}

#[test]
fn a_key_over_https_starts_normally() {
    let out = start_with("https://gateway.example", Some("sk-llm-secret"));
    assert_reached_the_db(&out, "https with a key");
}

#[test]
fn a_key_over_plaintext_loopback_starts_normally() {
    let out = start_with("http://127.0.0.1:1234", Some("sk-llm-secret"));
    assert_reached_the_db(&out, "loopback with a key");
}

#[test]
fn a_keyless_plaintext_non_loopback_endpoint_starts_normally() {
    let out = start_with("http://192.168.1.10:1234", None);
    assert_reached_the_db(&out, "non-loopback LAN endpoint without a key");
}
