// Unlike integration_server.rs (router-level oneshot, no socket), this test
// exercises the real TLS transport: binds a std TcpListener, adopts it with
// axum_server::from_tcp_rustls exactly as main::run does, and drives a real
// HTTPS request to /v1/health over the loopback socket.
//
// Self-signed cert/key are generated at test time via openssl and never
// committed; if openssl isn't on PATH, the TLS body is skipped rather than
// failing CI images without it.

use crate::common;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

use axum_server::tls_rustls::RustlsConfig;
use inkentry_core::config::apply_server_ca;
use inkentry_server::router;
use serial_test::serial;

// Mint a throwaway self-signed leaf (CN=localhost, SAN IP 127.0.0.1) into
// `dir`; returns None when openssl is absent so the caller can skip.
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
        Err(_) => None,
    }
}

// Also proves the bind-before-warm guarantee for the TLS branch: the socket
// is bound and made non-blocking before from_tcp_rustls adopts it, so health
// is served off the pre-bound fd — the same single bind point the plaintext
// path uses, before main::run would warm the embedder.
#[tokio::test]
#[serial]
async fn health_over_real_https() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((cert, key)) = make_self_signed(dir.path()) else {
        eprintln!("SKIP health_over_real_https: openssl not found on PATH");
        return;
    };

    // Install `ring` as the process crypto provider (mirrors main::run);
    // ignore the error if another test already installed one.
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

// Mints an internal CA plus a leaf server cert it signs (CA:TRUE / CA:FALSE
// with serverAuth + SAN IP:127.0.0.1,DNS:localhost), or None if openssl is
// absent. A bare self-signed cert is itself CA:TRUE and webpki rejects it as
// an end-entity, so a real CA chain is what this test's verification needs.
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
        Err(_) => return None,
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

// End-to-end proof of the custom-CA trust path (apply_server_ca /
// INKENTRY_SERVER_CA). Stands up real TLS with a leaf cert signed by an
// internal CA, then contrasts three reqwest clients against the same server:
// default roots only fails verification, a client built via
// apply_server_ca(ca) succeeds, and the INKENTRY_SERVER_CA env path succeeds
// the same way. Verification stays on throughout — apply_server_ca only adds
// a trust anchor, and the untrusted-client control confirms this isn't
// danger_accept_invalid_certs.
#[tokio::test]
#[serial]
async fn server_ca_establishes_trust_over_real_https() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((cert, leaf_cert, key)) = make_ca_and_leaf(dir.path()) else {
        eprintln!("SKIP server_ca_establishes_trust_over_real_https: openssl not found on PATH");
        return;
    };

    // Install `ring` as the process crypto provider (mirrors main::run).
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

    // Retry until the spawned server is accepting.
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

    // Control: default roots only. The server is already accepting, so a
    // failure here is a genuine trust rejection, not a connect race.
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

// main::run loads the cert/key before binding, so a bad cert is a startup
// error, never a half-up server.
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
