// Drives the real binary against a real rustls listener signed by an in-test
// CA: a plaintext wiremock never exercises certificate trust, which is how a
// broken `server_ca` setup once reported only "unreachable".

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// CA:TRUE; its own cert/key are usable directly as a broken leaf.
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
        .push(DnType::CommonName, "inkentry-tls-trust-test CA");
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

fn new_expired_leaf(issuer: &Issuer<'static, KeyPair>) -> (String, String) {
    let mut params = CertificateParams::new(vec!["127.0.0.1".to_string()]).expect("valid leaf SAN");
    params
        .distinguished_name
        .push(DnType::CommonName, "127.0.0.1");
    params.use_authority_key_identifier_extension = true;
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    params.not_before = date_time_ymd(2000, 1, 1);
    params.not_after = date_time_ymd(2001, 1, 1);

    let key_pair = KeyPair::generate().expect("generate leaf key");
    let cert = params
        .signed_by(&key_pair, issuer)
        .expect("sign expired leaf with CA");
    (cert.pem(), key_pair.serialize_pem())
}

async fn health_handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": "test",
        "capabilities": ["memory"],
        "embedding_dim": 0,
    }))
}

// Detached thread with its own runtime; it dies with the test binary.
fn spawn_tls_server(cert_pem: String, key_pem: String) -> u16 {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tls listener");
    let port = std_listener.local_addr().expect("local_addr").port();
    std_listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime for tls test server");
        rt.block_on(async move {
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

    // The socket is already listening; this only covers the accept loop's cold
    // start, since the probe against an explicit server_url is a single attempt.
    std::thread::sleep(std::time::Duration::from_millis(150));
    port
}

// Built offline (`INKENTRY_NO_SERVER=1`) so the later `status` run exercises
// only the probe against our TLS listener.
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

    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    (temp, project_dir, config_path)
}

// `server_url` is honored only from a project-level config (or env), never the
// global file. A loopback address means `project_id` is not required.
fn write_tls_config(
    config_path: &Path,
    db_path: &Path,
    port: u16,
    ca_pem_path: &Path,
    project_dir: &Path,
) {
    let cfg = format!(
        "db_path = {:?}\nserver_ca = {:?}\n",
        db_path.display().to_string(),
        ca_pem_path.display().to_string(),
    );
    fs::write(config_path, cfg).expect("write tls config");
    plumbing_helpers::write_project_server_config(
        project_dir,
        &format!("https://127.0.0.1:{port}"),
        "",
    );
}

fn write_tls_config_no_ca(config_path: &Path, db_path: &Path, port: u16, project_dir: &Path) {
    let cfg = format!("db_path = {:?}\n", db_path.display().to_string());
    fs::write(config_path, cfg).expect("write tls config with no server_ca");
    plumbing_helpers::write_project_server_config(
        project_dir,
        &format!("https://127.0.0.1:{port}"),
        "",
    );
}

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
        &project_dir,
    );

    let output = inkentry_bin()
        .current_dir(&project_dir)
        .env_remove("INKENTRY_NO_SERVER")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .expect("run inkentry status");

    assert!(
        output.status.success(),
        "inkentry status --format json exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON stdout");
    assert_eq!(
        body["tier"], "server",
        "CLI must reach Tier::Server over a properly CA-signed TLS listener; got: {body}"
    );
}

// A CA:TRUE certificate served as the listener's own leaf: the CLI must stay
// offline and name the certificate cause, not just report `[unreachable]`.
#[test]
fn tls_server_with_ca_cert_as_leaf_names_the_cause_not_just_unreachable() {
    let ca = new_ca();
    let port = spawn_tls_server(ca.cert_pem.clone(), ca.key_pem.clone());

    let (temp, project_dir, config_path) = setup_project();
    let ca_pem_path = temp.path().join("ca.pem");
    fs::write(&ca_pem_path, &ca.cert_pem).expect("write ca.pem");
    write_tls_config(
        &config_path,
        &temp.path().join("index.db"),
        port,
        &ca_pem_path,
        &project_dir,
    );

    let output = inkentry_bin()
        .current_dir(&project_dir)
        .env_remove("INKENTRY_NO_SERVER")
        .env("RUST_LOG", "inkentry=warn")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .output()
        .expect("run inkentry status");

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
        combined.contains("was presented as the server's own leaf certificate"),
        "output must name describe_rustls_error's CA-as-leaf sentence \
         specifically, not just cert_trust_hint's similarly-worded text (the \
         hint is present regardless of which cause matched, so checking only \
         for 'CA certificate'/'leaf certificate' would pass even if the \
         CaUsedAsEndEntity string-match broke and the cause fell through to \
         the generic 'certificate rejected: ...' branch): {combined}"
    );
    // tracing writes to stdout by default, so the WARN lands there, not stderr.
    assert!(
        combined.contains("full error chain"),
        "the WARN must include the full source chain, not just reqwest's \
         flattened top-level message: {combined}"
    );
    assert!(
        combined.contains("server-setup.md"),
        "with server_ca configured, the WARN must point at the client-trust \
         doc section: {combined}"
    );
}

// The CA is trusted, so this exercises rustls's own expiry check rather than
// issuer trust.
#[test]
fn tls_server_with_expired_leaf_names_expired_cause() {
    let ca = new_ca();
    let (leaf_pem, leaf_key_pem) = new_expired_leaf(&ca.issuer);
    let port = spawn_tls_server(leaf_pem, leaf_key_pem);

    let (temp, project_dir, config_path) = setup_project();
    let ca_pem_path = temp.path().join("ca.pem");
    fs::write(&ca_pem_path, &ca.cert_pem).expect("write ca.pem");
    write_tls_config(
        &config_path,
        &temp.path().join("index.db"),
        port,
        &ca_pem_path,
        &project_dir,
    );

    let output = inkentry_bin()
        .current_dir(&project_dir)
        .env_remove("INKENTRY_NO_SERVER")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .output()
        .expect("run inkentry status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Offline"),
        "must stay offline against an expired leaf; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("[tls:") && stdout.to_lowercase().contains("expired"),
        "status line must name the expired-certificate cause, not a generic \
         label; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("[unreachable]"),
        "a reachable server with an expired cert is not '[unreachable]'; stdout:\n{stdout}"
    );
}

// `server_ca` is deliberately unset, so the CLI trusts only the default roots.
// The `server_ca`-specific hint must be absent: it names a misconfiguration
// that does not apply.
#[test]
fn tls_server_with_untrusted_cert_and_no_server_ca_configured_names_cause_without_hint() {
    let ca = new_ca();
    let (leaf_pem, leaf_key_pem) = new_leaf(&ca.issuer);
    let port = spawn_tls_server(leaf_pem, leaf_key_pem);

    let (temp, project_dir, config_path) = setup_project();
    write_tls_config_no_ca(
        &config_path,
        &temp.path().join("index.db"),
        port,
        &project_dir,
    );

    let output = inkentry_bin()
        .current_dir(&project_dir)
        .env_remove("INKENTRY_NO_SERVER")
        .env("RUST_LOG", "inkentry=warn")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .output()
        .expect("run inkentry status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        stdout.contains("Offline"),
        "must stay offline against an untrusted issuer; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("[tls:") && stdout.to_lowercase().contains("unknown issuer"),
        "status line must name the unknown-issuer cause; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("[unreachable]"),
        "a reachable server with an untrusted cert is not '[unreachable]'; stdout:\n{stdout}"
    );
    assert!(
        !combined.contains("server_ca is configured") && !combined.contains("server-setup.md"),
        "the server_ca-specific hint must not appear when server_ca isn't set: {combined}"
    );
}

// `reqwest` reports a TLS handshake failure as a connect error, so telling it
// from an absent server takes more than `is_connect()`.
#[test]
fn cloud_first_write_to_an_untrusted_certificate_is_a_certificate_error_not_unreachable() {
    // Signed by a CA this machine does not trust, with no `server_ca`: the
    // ordinary internal-CA setup.
    let ca = new_ca();
    let (leaf_pem, leaf_key_pem) = new_leaf(&ca.issuer);
    let port = spawn_tls_server(leaf_pem, leaf_key_pem);
    let (temp, project_dir, config_path) = setup_project();
    let db_path = temp.path().join("index.db");
    write_tls_config_no_ca(&config_path, &db_path, port, &project_dir);
    let mut cfg = fs::read_to_string(&config_path).expect("read config");
    cfg.push_str("mode = \"cloud_first\"\n");
    fs::write(&config_path, cfg).expect("write cloud_first config");
    // Memory routed to a server needs a project to key entries to, and a raw
    // UUID skips slug resolution so the only thing left to fail is the
    // handshake under test.
    plumbing_helpers::write_project_server_config(
        &project_dir,
        &format!("https://127.0.0.1:{port}"),
        "11111111-1111-1111-1111-111111111111",
    );

    let out = inkentry_bin()
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(temp.path().join("memory.db"))
        .args(["add", "--kind", "note", "--title", "t", "--body", "b"])
        .output()
        .expect("run memory add");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "the write must fail: {stderr}");
    assert!(
        stderr.contains("TLS handshake"),
        "the error must name the handshake as what failed: {stderr}"
    );
    assert!(
        stderr.contains("unknown issuer"),
        "the error must carry the certificate cause: {stderr}"
    );
    assert!(
        stderr.contains("server_ca") || stderr.contains("INKENTRY_SERVER_CA"),
        "the error must name the setting that fixes it: {stderr}"
    );
    assert!(
        !stderr.contains("unreachable"),
        "a server that answered must not be reported as unreachable: {stderr}"
    );
}
