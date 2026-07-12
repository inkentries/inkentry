//! Integration test for the ADR-066 in-process HTTPS serve path.
//!
//! Unlike `integration_server.rs` (router-level oneshot, no socket), this test
//! exercises the *real* TLS transport: it binds a std `TcpListener`, adopts it
//! with `axum_server::from_tcp_rustls` exactly as `main::run` does, and drives a
//! full HTTPS request to `/v1/health` over the loopback socket.
//!
//! The self-signed cert/key are generated at test time into a tempdir via
//! `openssl` and never committed. If `openssl` is not on PATH the TLS body is
//! skipped (the machine can't mint a cert), so CI images without openssl don't
//! hard-fail.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

use axum_server::tls_rustls::RustlsConfig;
use serial_test::serial;
use spelunk_server::router;

/// Mint a throwaway self-signed leaf (CN=localhost, SAN IP 127.0.0.1) into
/// `dir`, returning `(cert_pem, key_pem)` paths. Returns `None` when `openssl`
/// is absent so the caller can skip rather than fail.
fn make_self_signed(dir: &Path) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let out = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=IP:127.0.0.1,DNS:localhost",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output();
    match out {
        Ok(o) if o.status.success() => Some((cert, key)),
        Ok(o) => panic!(
            "openssl failed to generate a self-signed cert: {}",
            String::from_utf8_lossy(&o.stderr)
        ),
        // openssl not installed → skip.
        Err(_) => None,
    }
}

/// The full remote path end to end: bind a std listener, adopt it for TLS via
/// the same `from_tcp_rustls` call `main::run` uses, and confirm a client gets a
/// 200 from `/v1/health` over real HTTPS.
///
/// This is also the bind-before-warm guarantee for the TLS branch: the socket is
/// bound (and made non-blocking) *before* `from_tcp_rustls` adopts it, so health
/// is served off the pre-bound fd — the same single bind point the plaintext
/// path uses, and the point at which `main::run` would have already returned
/// from `TcpListener::bind` before warming the embedder.
#[tokio::test]
#[serial]
async fn health_over_real_https() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((cert, key)) = make_self_signed(dir.path()) else {
        eprintln!("SKIP health_over_real_https: openssl not found on PATH");
        return;
    };

    // Install `ring` as the process crypto provider (mirrors `main::run`); the
    // reqwest client below also resolves against this process default. Ignore
    // the error if another test already installed one.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let state = common::make_test_state(4, None);
    let app = router(state);

    // Bind first, exactly like `main::run`: a std listener, made non-blocking so
    // tokio/axum-server can adopt the fd.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.set_nonblocking(true).expect("non-blocking");
    let addr = listener.local_addr().expect("local_addr");

    let config = RustlsConfig::from_pem_file(&cert, &key)
        .await
        .expect("load self-signed TLS material");

    // Serve on the pre-bound listener via the production TLS branch.
    let server = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, config)
            .expect("adopt std listener for TLS")
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
    });

    // Client trusts any cert (self-signed) but still performs a real TLS
    // handshake — a plaintext server would be rejected here.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("reqwest client");

    let url = format!("https://{addr}/v1/health");
    // Small retry loop: the spawned server may not have entered `accept` yet.
    let mut last_err = None;
    let mut status = None;
    for _ in 0..50 {
        match client.get(&url).send().await {
            Ok(resp) => {
                status = Some(resp.status());
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }

    server.abort();

    let status = status.unwrap_or_else(|| {
        panic!("no HTTPS response from {url}: {last_err:?}");
    });
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "GET {url} over HTTPS must return 200"
    );
}

/// Fail-fast on bad TLS material: `main::run` loads the cert/key *before* binding
/// so a bad cert is a startup error, never a half-up server. A non-existent cert
/// path must therefore make `RustlsConfig::from_pem_file` err.
#[tokio::test]
async fn tls_config_missing_cert_fails_fast() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing_cert = dir.path().join("nope-cert.pem");
    let missing_key = dir.path().join("nope-key.pem");
    let res = RustlsConfig::from_pem_file(&missing_cert, &missing_key).await;
    assert!(
        res.is_err(),
        "loading a non-existent cert/key must fail fast, not silently succeed"
    );
}
