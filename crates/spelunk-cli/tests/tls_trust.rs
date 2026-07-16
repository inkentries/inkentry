//! End-to-end coverage for the CLI's real TLS-server client path.
//!
//! Every existing capability-probe test talks to a plaintext wiremock server;
//! none ever drove a genuine TLS handshake through the CLI's real `reqwest`
//! client. That gap is exactly how a broken `server_ca` setup shipped
//! invisible: the server was reachable (`curl` returned 200), only
//! certificate trust failed, and the CLI reported "unreachable" with no
//! further detail.
//!
//! This spins up a real axum-server rustls listener signed by an in-test CA
//! (rcgen) and drives the actual `spelunk` binary against it over
//! `https://127.0.0.1:<port>`:
//!
//! - a proper CA -> leaf chain: the CLI must reach `Tier::Server`.
//! - the classic self-hosting.md client-trust trap (a CA:TRUE certificate
//!   served as the listener's own leaf): the CLI must stay offline AND name
//!   the certificate cause, not just report "unreachable".

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// ── cert generation ──────────────────────────────────────────────────────────

/// A self-signed CA (CA:TRUE, SAN `127.0.0.1`). Returns the CA's own
/// `(cert_pem, key_pem)` (usable directly as a broken leaf) plus an `Issuer`
/// handle for signing a proper leaf.
struct TestCa {
    cert_pem: String,
    key_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

fn new_ca() -> TestCa {
    let mut params = CertificateParams::new(vec!["127.0.0.1".to_string()]).expect("valid CA SAN");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "spelunk-tls-trust-test CA");
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    params.key_usages.push(KeyUsagePurpose::CrlSign);

    let key_pair = KeyPair::generate().expect("generate CA key");
    let cert = params.clone().self_signed(&key_pair).expect("self-sign CA");
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    let issuer = Issuer::new(params, key_pair);

    TestCa {
        cert_pem,
        key_pem,
        issuer,
    }
}

/// Issue a proper `127.0.0.1` leaf (CA:FALSE, serverAuth EKU) from `issuer`.
/// Returns `(cert_pem, key_pem)` for the TLS listener.
fn new_leaf(issuer: &Issuer<'static, KeyPair>) -> (String, String) {
    let mut params = CertificateParams::new(vec!["127.0.0.1".to_string()]).expect("valid leaf SAN");
    params
        .distinguished_name
        .push(DnType::CommonName, "127.0.0.1");
    params.use_authority_key_identifier_extension = true;
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);

    let key_pair = KeyPair::generate().expect("generate leaf key");
    let cert = params
        .signed_by(&key_pair, issuer)
        .expect("sign leaf with CA");
    (cert.pem(), key_pair.serialize_pem())
}

// ── TLS listener ─────────────────────────────────────────────────────────────

async fn health_handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": "test",
        "capabilities": ["memory"],
        "embedding_dim": 0,
    }))
}

/// Spawn a real axum-server rustls TLS listener on `127.0.0.1` serving
/// `GET /v1/health`, on its own thread with its own Tokio runtime. The thread
/// is detached (never joined) but spawns no separate OS process, so it dies
/// with the test binary; nothing survives the test run to sweep. Returns the
/// bound port.
fn spawn_tls_server(cert_pem: String, key_pem: String) -> u16 {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tls listener");
    let port = std_listener.local_addr().expect("local_addr").port();
    std_listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime for tls test server");
        rt.block_on(async move {
            // Same provider choice as spelunk-server's own TLS listener (ADR-066).
            let _ = rustls::crypto::ring::default_provider().install_default();
            let config = axum_server::tls_rustls::RustlsConfig::from_pem(
                cert_pem.into_bytes(),
                key_pem.into_bytes(),
            )
            .await
            .expect("build rustls config from generated cert/key");

            let app = axum::Router::new().route("/v1/health", axum::routing::get(health_handler));
            axum_server::from_tcp_rustls(std_listener, config)
                .expect("adopt std listener for tls")
                .serve(app.into_make_service())
                .await
                .expect("serve tls listener");
        });
    });

    // The socket is already bound+listening (kernel backlog accepts
    // immediately); this only covers the accept loop's cold start, since the
    // CLI's probe against an explicit server_url is a single attempt with no
    // retry.
    std::thread::sleep(std::time::Duration::from_millis(150));
    port
}

// ── project setup ────────────────────────────────────────────────────────────

/// Build a minimal indexed project, entirely offline (`SPELUNK_NO_SERVER=1`),
/// so the later `status` run only exercises the probe against our own TLS
/// listener, not real embedding.
fn setup_project() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).expect("mkdir project");
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn hello() -> &'static str { \"hello\" }",
    )
    .expect("write fixture file");

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");
    fs::write(
        &config_path,
        format!("db_path = {:?}\n", db_path.display().to_string()),
    )
    .expect("write initial config");

    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    (temp, project_dir, config_path)
}

/// Overwrite `config_path` to point at `server_url` (our TLS test listener),
/// trusting `ca_pem_path` via `server_ca`.
fn write_tls_config(config_path: &Path, db_path: &Path, port: u16, ca_pem_path: &Path) {
    let cfg = format!(
        "db_path = {:?}\nserver_url = \"https://127.0.0.1:{port}\"\nserver_ca = {:?}\n",
        db_path.display().to_string(),
        ca_pem_path.display().to_string(),
    );
    fs::write(config_path, cfg).expect("write tls config");
}

// ── tests ────────────────────────────────────────────────────────────────────

/// A properly issued CA -> leaf chain: the CLI's real client path must reach
/// `Tier::Server` (the CA-trust config working as documented).
#[test]
fn tls_server_with_proper_ca_chain_reaches_server_tier() {
    let ca = new_ca();
    let (leaf_pem, leaf_key_pem) = new_leaf(&ca.issuer);
    let port = spawn_tls_server(leaf_pem, leaf_key_pem);

    let (temp, project_dir, config_path) = setup_project();
    let ca_pem_path = temp.path().join("ca.pem");
    fs::write(&ca_pem_path, &ca.cert_pem).expect("write ca.pem");
    write_tls_config(
        &config_path,
        &temp.path().join("index.db"),
        port,
        &ca_pem_path,
    );

    let output = spelunk_bin()
        .current_dir(&project_dir)
        .env_remove("SPELUNK_NO_SERVER")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .expect("run spelunk status");

    assert!(
        output.status.success(),
        "spelunk status --format json exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON stdout");
    assert_eq!(
        body["tier"], "server",
        "CLI must reach Tier::Server over a properly CA-signed TLS listener; got: {body}"
    );
}

/// The classic self-hosting.md client-trust trap: the server presents a
/// CA:TRUE certificate as its own leaf. The CLI must stay offline, but must
/// distinguish "reachable, TLS trust failed" from a plain "[unreachable]",
/// and must name the certificate cause in both the WARN and the status line.
#[test]
fn tls_server_with_ca_cert_as_leaf_names_the_cause_not_just_unreachable() {
    let ca = new_ca();
    // Serve the CA certificate itself (CA:TRUE) as the listener's own leaf.
    let port = spawn_tls_server(ca.cert_pem.clone(), ca.key_pem.clone());

    let (temp, project_dir, config_path) = setup_project();
    let ca_pem_path = temp.path().join("ca.pem");
    fs::write(&ca_pem_path, &ca.cert_pem).expect("write ca.pem");
    write_tls_config(
        &config_path,
        &temp.path().join("index.db"),
        port,
        &ca_pem_path,
    );

    let output = spelunk_bin()
        .current_dir(&project_dir)
        .env_remove("SPELUNK_NO_SERVER")
        .env("RUST_LOG", "spelunk=warn")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .output()
        .expect("run spelunk status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        stdout.contains("Offline"),
        "must stay offline against a CA-as-leaf server; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("[tls:"),
        "status line must distinguish 'reachable, TLS trust failed' from \
         plain '[unreachable]'; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("[unreachable]"),
        "a server that answered the TLS handshake (even if untrusted) is not \
         '[unreachable]': that label is reserved for TCP/connect failures; stdout:\n{stdout}"
    );
    assert!(
        combined.contains("CA certificate") && combined.contains("leaf certificate"),
        "output must name the actual certificate cause (CA used as leaf), not \
         collapse to a bare 'unreachable': {combined}"
    );
    // tracing's fmt subscriber writes to stdout by default, so the WARN lands
    // there, not on stderr; check the combined output either way.
    assert!(
        combined.contains("full error chain"),
        "the WARN must include the full source chain, not just reqwest's \
         flattened top-level message: {combined}"
    );
    assert!(
        combined.contains("self-hosting.md"),
        "with server_ca configured, the WARN must point at the client-trust \
         doc section: {combined}"
    );
}
