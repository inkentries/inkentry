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

use crate::common;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

use axum_server::tls_rustls::RustlsConfig;
use inkentry_core::config::apply_server_ca;
use inkentry_server::router;
use serial_test::serial;

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

/// Mint an internal CA plus a leaf server cert signed by it into `dir`, the
/// faithful "internal-CA team server" shape: the CA is `CA:TRUE`, the leaf is
/// `CA:FALSE` with `serverAuth` and SAN `IP:127.0.0.1,DNS:localhost`. Returns
/// `(ca_cert_pem, leaf_cert_pem, leaf_key_pem)` paths, or `None` if `openssl` is
/// absent so the caller can skip. (A bare self-signed `req -x509` cert is
/// `CA:TRUE` and webpki rejects it as an end-entity — `CaUsedAsEndEntity` — so a
/// real internal-CA chain, not a single self-signed cert, is what verification
/// exercises.)
fn make_ca_and_leaf(
    dir: &Path,
) -> Option<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)> {
    let ca_cert = dir.join("ca.pem");
    let ca_key = dir.join("ca.key");
    let leaf_cert = dir.join("leaf.pem");
    let leaf_key = dir.join("leaf.key");
    let leaf_csr = dir.join("leaf.csr");
    let ext = dir.join("leaf.ext");
    std::fs::write(
        &ext,
        "subjectAltName=IP:127.0.0.1,DNS:localhost\n\
         basicConstraints=CA:FALSE\n\
         keyUsage=digitalSignature,keyEncipherment\n\
         extendedKeyUsage=serverAuth\n",
    )
    .expect("write leaf ext");

    // Root CA (self-signed, CA:TRUE).
    let ca = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=inkentry-test-ca",
            "-keyout",
        ])
        .arg(&ca_key)
        .arg("-out")
        .arg(&ca_cert)
        .output();
    match ca {
        Ok(o) if o.status.success() => {}
        Ok(o) => panic!(
            "openssl CA gen failed: {}",
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(_) => return None, // openssl absent → skip.
    }
    // Leaf key + CSR.
    let csr = Command::new("openssl")
        .args([
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-keyout",
        ])
        .arg(&leaf_key)
        .arg("-out")
        .arg(&leaf_csr)
        .output()
        .expect("openssl leaf CSR");
    assert!(
        csr.status.success(),
        "leaf CSR: {}",
        String::from_utf8_lossy(&csr.stderr)
    );
    // Sign the leaf with the CA, applying the serverAuth/SAN extensions.
    let sign = Command::new("openssl")
        .args(["x509", "-req", "-days", "1", "-CAcreateserial", "-in"])
        .arg(&leaf_csr)
        .arg("-CA")
        .arg(&ca_cert)
        .arg("-CAkey")
        .arg(&ca_key)
        .arg("-extfile")
        .arg(&ext)
        .arg("-out")
        .arg(&leaf_cert)
        .output()
        .expect("openssl leaf sign");
    assert!(
        sign.status.success(),
        "leaf sign: {}",
        String::from_utf8_lossy(&sign.stderr)
    );
    Some((ca_cert, leaf_cert, leaf_key))
}

/// End-to-end proof of the custom-CA trust path (`config::apply_server_ca` /
/// `INKENTRY_SERVER_CA`). Stands up the real TLS transport with a leaf cert
/// signed by an internal CA, then contrasts three reqwest clients against the
/// *same* server:
///   - default roots only                 → TLS verification FAILS (untrusted),
///   - built via `apply_server_ca(ca)`     → handshake succeeds, `/v1/health` 200s,
///   - the `INKENTRY_SERVER_CA`-provided path → same success.
///
/// Verification stays ON throughout — `apply_server_ca` only adds the CA as a
/// trust anchor; the untrusted-client control confirms this isn't
/// `danger_accept_invalid_certs`. (env→config precedence itself is covered by
/// the config unit tests.)
#[tokio::test]
#[serial]
async fn server_ca_establishes_trust_over_real_https() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((cert, leaf_cert, key)) = make_ca_and_leaf(dir.path()) else {
        eprintln!("SKIP server_ca_establishes_trust_over_real_https: openssl not found on PATH");
        return;
    };

    // Install `ring` as the process crypto provider (mirrors `main::run`).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let state = common::make_test_state(4, None);
    let app = router(state);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.set_nonblocking(true).expect("non-blocking");
    let addr = listener.local_addr().expect("local_addr");

    // Server presents the CA-signed *leaf*; the client trusts the *CA*.
    let config = RustlsConfig::from_pem_file(&leaf_cert, &key)
        .await
        .expect("load leaf TLS material");

    let server = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, config)
            .expect("adopt std listener for TLS")
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
    });

    let url = format!("https://{addr}/v1/health");
    let timeout = std::time::Duration::from_secs(10);

    // Trusting client via `apply_server_ca(config `server_ca`)`. Retry until the
    // spawned server is accepting; a 200 here is the success half of the proof.
    let trusting = apply_server_ca(reqwest::Client::builder().timeout(timeout), Some(&cert))
        .expect("cert PEM is a valid CA bundle")
        .build()
        .expect("reqwest client");
    let mut status = None;
    let mut last_err = None;
    for _ in 0..50 {
        match trusting.get(&url).send().await {
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
    let status = status.unwrap_or_else(|| {
        panic!("apply_server_ca client got no response from {url}: {last_err:?}")
    });
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "client trusting the internal CA must reach {url}"
    );

    // Control: only the built-in roots. The server is already accepting, so a
    // failure here is a genuine trust rejection, not a connect race — this is
    // what proves verification stayed on.
    let untrusting = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client");
    let err = untrusting.get(&url).send().await.expect_err(
        "a client without the custom CA must fail TLS verification against the internal-CA server",
    );
    eprintln!("untrusted client rejected as expected: {err}");

    // Env path: `INKENTRY_SERVER_CA` supplies the same PEM, read exactly as
    // `Config::load` does, then routed through `apply_server_ca`.
    unsafe { std::env::set_var("INKENTRY_SERVER_CA", &cert) };
    let env_ca = std::env::var("INKENTRY_SERVER_CA").expect("INKENTRY_SERVER_CA set");
    unsafe { std::env::remove_var("INKENTRY_SERVER_CA") };
    let via_env = apply_server_ca(
        reqwest::Client::builder().timeout(timeout),
        Some(Path::new(&env_ca)),
    )
    .expect("env CA bundle accepted")
    .build()
    .expect("reqwest client");
    let env_status = via_env
        .get(&url)
        .send()
        .await
        .expect("env-CA client reaches server")
        .status();
    assert_eq!(
        env_status,
        reqwest::StatusCode::OK,
        "INKENTRY_SERVER_CA path must also establish trust"
    );

    server.abort();
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
