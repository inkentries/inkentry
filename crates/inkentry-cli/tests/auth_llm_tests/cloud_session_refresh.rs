use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, header, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROJECT_ID: &str = "test-org/test-project";
const STALE_TOKEN: &str = "at-stale";
const STALE_REFRESH: &str = "rt-stale";
const ROTATED_REFRESH: &str = "rt-rotated";
const SERVER_TITLE: &str = "entry stored on the server";

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

fn fresh_jwt() -> String {
    let claims = serde_json::json!({ "exp": 5_000_000_000_i64, "org_id": "org_1" });
    format!(
        "{}.{}.sig",
        b64url(br#"{"alg":"none"}"#),
        b64url(claims.to_string().as_bytes())
    )
}

fn secrets_path(home: &Path) -> std::path::PathBuf {
    home.join(".config").join("inkentry").join("secrets.toml")
}

fn seed_session(home: &Path, cloud_origin: &str, expires_at: i64) {
    let dir = home.join(".config").join("inkentry");
    std::fs::create_dir_all(&dir).unwrap();
    let payload = serde_json::json!({
        "active": "org_1",
        "orgs": {
            "org_1": {
                "access_token": STALE_TOKEN,
                "refresh_token": STALE_REFRESH,
                "expires_at": expires_at,
                "org_id": "org_1",
                "cloud_origin": cloud_origin,
            },
        }
    });
    let path = secrets_path(home);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, format!("org_tokens = '{payload}'\n{existing}")).unwrap();
}

async fn memory_server(accepted_bearer: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory", "memory.stream"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/[^/]+/memory$"))
        .and(header("authorization", format!("Bearer {accepted_bearer}")))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33",
            "kind": "decision",
            "title": "t",
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/projects/[^/]+/memory$"))
        .and(header("authorization", format!("Bearer {accepted_bearer}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33",
                "kind": "decision",
                "title": SERVER_TITLE,
            }],
            "total": 1,
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path_regex(r"^/v1/projects/[^/]+/memory$"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(2)
        .mount(&server)
        .await;
    server
}

async fn workos_server(access_token: &str, expected_calls: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/user_management/authenticate"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!(
            "refresh_token={STALE_REFRESH}"
        )))
        .and(body_string_contains("organization_id=org_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": access_token,
            "refresh_token": ROTATED_REFRESH,
            "organization_id": "org_1",
        })))
        .expect(expected_calls)
        .mount(&server)
        .await;
    server
}

fn memory_cmd(
    home: &Path,
    work: &Path,
    server_url: &str,
    workos_url: &str,
    args: &[&str],
) -> std::process::Output {
    // `mode` is read only from the global config.
    let config_path = work.join("config.toml");
    std::fs::write(&config_path, "mode = \"cloud_first\"\n").unwrap();
    inkentry_bin_in(home)
        .current_dir(work)
        .env_remove("INKENTRY_SERVER_KEY")
        .env("INKENTRY_SERVER_URL", server_url)
        .env("INKENTRY_PROJECT_ID", PROJECT_ID)
        .env("INKENTRY_WORKOS_URL", workos_url)
        .env("INKENTRY_WORKOS_CLIENT_ID", "client_test")
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(work.join("memory.db"))
        .args(args)
        .output()
        .unwrap()
}

fn memory_add(
    home: &Path,
    work: &Path,
    server_url: &str,
    workos_url: &str,
) -> std::process::Output {
    let args = ["add", "--kind", "decision", "--title", "t", "--body", "b"];
    memory_cmd(home, work, server_url, workos_url, &args)
}

fn memory_posts(requests: &[wiremock::Request]) -> Vec<Option<String>> {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/memory"))
        .map(|r| {
            r.headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_string())
        })
        .collect()
}

// Exactly one rotation: the inference client and the memory backend both see
// the expired session, and the second must reuse the one the first persisted.
#[tokio::test]
async fn memory_add_refreshes_an_expired_cloud_session_before_its_first_request() {
    let fresh = fresh_jwt();
    let server = memory_server(&fresh).await;
    let workos = workos_server(&fresh, 1).await;
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    seed_session(home.path(), &server.uri(), 0);

    let out = memory_add(home.path(), work.path(), &server.uri(), &workos.uri());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "memory add must succeed: {stderr}");

    assert_eq!(
        memory_posts(&server.received_requests().await.unwrap()),
        vec![Some(format!("Bearer {fresh}"))],
        "the expired token must never be sent; the first request carries the rotated one"
    );
    let secrets = std::fs::read_to_string(secrets_path(home.path())).unwrap();
    assert!(
        secrets.contains(ROTATED_REFRESH),
        "the rotated refresh token must be persisted"
    );
    assert!(
        !secrets.contains(STALE_REFRESH),
        "the spent refresh token must be replaced"
    );
}

// `add` also embeds through the inference client, which refreshes itself;
// `list` reaches the server only via the memory backend, so this pins that
// backend's own refresh.
#[tokio::test]
async fn memory_list_refreshes_an_expired_cloud_session_before_its_first_request() {
    let fresh = fresh_jwt();
    let server = memory_server(&fresh).await;
    let workos = workos_server(&fresh, 1).await;
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    seed_session(home.path(), &server.uri(), 0);

    let out = memory_cmd(
        home.path(),
        work.path(),
        &server.uri(),
        &workos.uri(),
        &["list", "--format", "json"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "memory list must succeed: {stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains(SERVER_TITLE));

    let stale_sent = server.received_requests().await.unwrap().iter().any(|r| {
        r.headers.get("authorization").map(|v| v.to_str().unwrap())
            == Some(&format!("Bearer {STALE_TOKEN}"))
    });
    assert!(!stale_sent, "the expired token must never reach the server");
}

// The token looks live locally but the server rejects it (revoked, clock skew).
#[tokio::test]
async fn memory_add_refreshes_and_retries_once_when_the_server_rejects_the_session() {
    let fresh = fresh_jwt();
    let server = memory_server(&fresh).await;
    let workos = workos_server(&fresh, 1).await;
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    seed_session(home.path(), &server.uri(), 4_000_000_000);

    let out = memory_add(home.path(), work.path(), &server.uri(), &workos.uri());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "memory add must succeed: {stderr}");

    assert_eq!(
        memory_posts(&server.received_requests().await.unwrap()),
        vec![
            Some(format!("Bearer {STALE_TOKEN}")),
            Some(format!("Bearer {fresh}")),
        ],
    );
}

// A session issued for another origin is never sent to, or refreshed for, a
// self-hosted server; its 401 surfaces with the set-key hint.
#[tokio::test]
async fn a_self_hosted_rejection_never_refreshes_the_cloud_session() {
    let server = memory_server("never-matches").await;
    let workos = workos_server("unused", 0).await;
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    seed_session(home.path(), "https://api.inkentry.com", 0);

    let out = memory_add(home.path(), work.path(), &server.uri(), &workos.uri());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a rejected credential must fail");
    assert!(
        stderr.contains("inkentry auth set-key --server"),
        "the error must name the fix for a self-hosted server: {stderr}"
    );
    for bearer in memory_posts(&server.received_requests().await.unwrap()) {
        assert!(
            bearer.is_none(),
            "the cloud session must not reach another origin: {bearer:?}"
        );
    }
}
