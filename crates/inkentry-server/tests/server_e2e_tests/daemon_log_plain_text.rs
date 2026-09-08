// The daemon log file is plain text.
//
// `inkentry server start` daemonises the server with stdout and stderr
// redirected into `server.log`, so anything the log layer colours is written
// into a file as raw escape bytes. This drives the real binary the same way
// and reads the bytes back.
//
// `--model-dir` points at an empty directory so the native embedder fails
// fast locally instead of reaching the Hugging Face Hub.

use std::process::{Command, Stdio};
use std::time::Duration;

const ESC: u8 = 0x1b;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_for_health(port: u16) {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Ok(r) = client
            .get(format!("http://127.0.0.1:{port}/v1/health"))
            .send()
            .await
            && r.status().is_success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("inkentry-server on port {port} never became healthy");
}

#[tokio::test]
async fn a_redirected_daemon_log_holds_no_escape_bytes() {
    let db_dir = tempfile::TempDir::new().unwrap();
    let model_dir = tempfile::TempDir::new().unwrap();
    let log_dir = tempfile::TempDir::new().unwrap();
    let log_path = log_dir.path().join("server.log");
    let log = std::fs::File::create(&log_path).unwrap();

    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_inkentry-server"))
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--db",
            db_dir.path().join("server.db").to_str().unwrap(),
            "--model-dir",
            model_dir.path().to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .env_remove("NO_COLOR")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .expect("spawning inkentry-server");

    wait_for_health(port).await;
    let _ = child.kill();
    let _ = child.wait();

    let bytes = std::fs::read(&log_path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    // Without this the escape assertion could pass on an empty file.
    assert!(
        text.contains("inkentry-server listening on"),
        "the daemon should have logged its startup line: {text}"
    );
    assert!(
        !bytes.contains(&ESC),
        "the log file must be plain text, but byte {:?} holds an escape: {text}",
        bytes.iter().position(|b| *b == ESC)
    );
}
