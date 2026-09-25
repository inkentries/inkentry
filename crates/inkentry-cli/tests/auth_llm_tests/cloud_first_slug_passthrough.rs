// A non-loopback server_url must be https://, so this drives a real rustls
// listener addressed via the non-loopback 0.0.0.0 alias.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const LOCAL_TITLE: &str = "local only entry";
const SERVER_TITLE: &str = "entry that only exists on the server";
const PROJECT_SLUG: &str = "github.com/owner/repo";

struct TestCa {
    cert_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

fn new_ca() -> TestCa {
    let mut params = CertificateParams::new(vec!["0.0.0.0".to_string()]).expect("valid CA SAN");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "inkentry-slug-passthrough-test CA");
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    params.key_usages.push(KeyUsagePurpose::CrlSign);

    let key_pair = KeyPair::generate().expect("generate CA key");
    let cert = params.clone().self_signed(&key_pair).expect("self-sign CA");
    let cert_pem = cert.pem();
    let issuer = Issuer::new(params, key_pair);
    TestCa { cert_pem, issuer }
}

fn new_leaf(issuer: &Issuer<'static, KeyPair>) -> (String, String) {
    let mut params = CertificateParams::new(vec!["0.0.0.0".to_string()]).expect("valid leaf SAN");
    params
        .distinguished_name
        .push(DnType::CommonName, "0.0.0.0");
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

// `paths` records every request so a reintroduced pre-flight shows up;
// `memory_segments` is the project segment as the server decoded it.
#[derive(Default)]
struct Seen {
    paths: Mutex<Vec<String>>,
    memory_segments: Mutex<Vec<String>>,
}

impl Seen {
    fn paths(&self) -> Vec<String> {
        self.paths.lock().expect("seen lock").clone()
    }

    fn memory_segments(&self) -> Vec<String> {
        self.memory_segments.lock().expect("seen lock").clone()
    }
}

// Detached thread that dies with the test binary. The listener is bound
// before this returns and handed to the server, so the port cannot be claimed
// in between.
fn spawn_tls_server(
    cert_pem: String,
    key_pem: String,
    memory: serde_json::Value,
    seen: Arc<Seen>,
) -> u16 {
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

            let memory_seen = Arc::clone(&seen);
            let fallback_seen = Arc::clone(&seen);
            let app = axum::Router::new()
                .route(
                    "/v1/projects/{project_id}/memory",
                    axum::routing::get(
                        move |axum::extract::Path(project_id): axum::extract::Path<String>,
                              uri: axum::http::Uri| {
                            let seen = Arc::clone(&memory_seen);
                            let body = memory.clone();
                            async move {
                                seen.paths
                                    .lock()
                                    .expect("seen lock")
                                    .push(uri.path().to_string());
                                seen.memory_segments
                                    .lock()
                                    .expect("seen lock")
                                    .push(project_id);
                                axum::Json(body)
                            }
                        },
                    ),
                )
                .fallback(move |uri: axum::http::Uri| {
                    let seen = Arc::clone(&fallback_seen);
                    async move {
                        seen.paths
                            .lock()
                            .expect("seen lock")
                            .push(uri.path().to_string());
                        axum::Json(serde_json::json!({}))
                    }
                });
            axum_server::from_tcp_rustls(std_listener, config)
                .expect("adopt std listener for tls")
                .serve(app.into_make_service())
                .await
                .expect("serve tls listener");
        });
    });

    std::thread::sleep(std::time::Duration::from_millis(150));
    port
}

fn oss_memory_list() -> serde_json::Value {
    serde_json::json!([{
        "id": "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33",
        "kind": "note",
        "title": SERVER_TITLE,
        "body": "b",
        "tags": [],
        "linked_files": [],
        "created_at": 1_700_000_000_i64,
        "status": "active",
        "superseded_by": null,
    }])
}

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

// Seed one entry into the local store, so a silent local fallback would be
// visible on stdout rather than indistinguishable from an empty result.
fn seeded_project() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let mem_path = db_path.with_file_name("memory.db");
    let cfg = write_cfg(tmp.path(), "config-seed.toml", &db_path, "");
    let out = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
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
    assert!(out.status.success());
    (tmp, mem_path)
}

// Build the `cloud_first` project: `mode` is read only from the global config,
// `server_url` / `project_id` only from the project-level `.inkentry/config.toml`.
fn cloud_first_project(ca_pem: &str, port: u16, project_id: &str) -> (TempDir, PathBuf, PathBuf) {
    let (tmp, mem_path) = seeded_project();
    let ca_path = tmp.path().join("ca.pem");
    std::fs::write(&ca_path, ca_pem).expect("write ca pem");

    let cfg = write_cfg(
        tmp.path(),
        "config-cloud-first.toml",
        &tmp.path().join("inkentry.db"),
        &format!(
            "mode = \"cloud_first\"\nserver_ca = {:?}\n",
            ca_path.display().to_string()
        ),
    );
    let server_url = format!("https://0.0.0.0:{port}");
    // Without this the test would keep passing if `0.0.0.0` were ever
    // reclassified as loopback, while silently no longer covering the
    // non-loopback peer it exists to prove.
    assert!(
        !inkentry_core::config::is_loopback_url(&server_url),
        "test seam precondition: {server_url} must be classified non-loopback"
    );
    plumbing_helpers::write_project_server_config(tmp.path(), &server_url, project_id);
    (tmp, mem_path, cfg)
}

fn memory_list(tmp: &TempDir, cfg: &Path, mem_path: &Path) -> std::process::Output {
    inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(cfg)
        .args(["memory", "--db"])
        .arg(mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap()
}

// A slug containing `/` must survive as one percent-encoded segment and
// decode back on the server.
//
// Connecting to `0.0.0.0` raises `WSAEADDRNOTAVAIL` on Windows.
#[test]
#[cfg_attr(windows, ignore)]
fn cloud_first_reads_remotely_with_the_configured_slug_verbatim() {
    let ca = new_ca();
    let (leaf_pem, leaf_key) = new_leaf(&ca.issuer);
    let seen = Arc::new(Seen::default());
    let port = spawn_tls_server(leaf_pem, leaf_key, oss_memory_list(), Arc::clone(&seen));

    let (tmp, mem_path, cfg) = cloud_first_project(&ca.cert_pem, port, PROJECT_SLUG);
    let out = memory_list(&tmp, &cfg, &mem_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "the documented self-hosted cloud_first config must work; stderr: {stderr}"
    );
    assert!(
        stdout.contains(SERVER_TITLE),
        "reads must come from the server in cloud_first: {stdout}"
    );
    assert!(
        !stdout.contains(LOCAL_TITLE),
        "the local store must not be read in cloud_first: {stdout}"
    );
    assert_eq!(
        seen.memory_segments(),
        vec![PROJECT_SLUG.to_string()],
        "the configured project_id must reach the server verbatim, in one segment"
    );
    // `/v1/health` is the dialect probe issued on every open; only project
    // lookups and extra reads are forbidden.
    assert_eq!(
        seen.paths()
            .into_iter()
            .filter(|p| p != "/v1/health")
            .collect::<Vec<_>>(),
        vec!["/v1/projects/github.com%2Fowner%2Frepo/memory".to_string()],
        "the memory read must be the only memory request the mode makes"
    );
}
