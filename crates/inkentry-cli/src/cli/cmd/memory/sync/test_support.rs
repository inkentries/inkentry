// Shared test fixtures for the `sync` module's submodule test suites
// (`super::push`, `super::pull`, `super::round`).

use crate::storage::MemoryStore;

pub(super) fn register_sqlite_vec() {
    use std::sync::OnceLock;
    // `MemoryStore::open` creates a vec0 table, so the extension must be
    // registered before any connection opens.
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

// Spin up a real `inkentry-server` axum router (the production router) on
// an ephemeral loopback port, serving the team-hosting
// `/v1/projects/*/memory*` routes this test's `CloudSyncClient`s talk to.
pub(super) async fn spawn_inkentry_server() -> std::net::SocketAddr {
    register_sqlite_vec();
    let db_dir = tempfile::TempDir::new().unwrap();
    let db = inkentry_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
        .unwrap();
    let instance_id = db.get_or_create_instance_id().unwrap();
    let state = inkentry_server::AppState {
        db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
        auth: std::sync::Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
        conflict_threshold: inkentry_server::default_conflict_threshold(),
        embedder: inkentry_server::EmbedderSlot::disabled(),
        embed_admission: inkentry_server::EmbedAdmission::new(
            inkentry_server::EMBED_QUEUE_CAPACITY,
            inkentry_server::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        embed_threads: 4,
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: std::sync::Arc::new(inkentry_server::rate_limiter::RateLimiter::new(
            1000, 60,
        )),
        instance_id,
        started_by: None,
        trusted_proxies: Default::default(),
        relay: inkentry_server::relay::RelayRegistry::disabled(),
        repair_signal: inkentry_server::repair::RepairSignal::new(),
    };
    let app = inkentry_server::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

// A mocked local inkentry-server standing in for the loopback embedder, wired
// up the way auto-discovery actually finds one: a `server.port` file under
// `INKENTRY_STATE_DIR` pointing at the mock. Going through the real discovery
// path (rather than injecting an `inference_url`) is what makes the
// "embed never reaches the team server_url" tests meaningful, and pins the
// probe to this mock instead of whatever happens to listen on port 4655 on the
// machine running the tests.
//
// Mutates process-global env, so every test using it must be `#[serial]`.
pub(super) struct LoopbackEmbedder {
    pub(super) server: wiremock::MockServer,
    _state_dir: tempfile::TempDir,
    prev_state_dir: Option<std::ffi::OsString>,
    prev_discovery_port: Option<std::ffi::OsString>,
    prev_no_server: Option<std::ffi::OsString>,
}

impl Drop for LoopbackEmbedder {
    fn drop(&mut self) {
        unsafe {
            match self.prev_state_dir.take() {
                Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                None => std::env::remove_var("INKENTRY_STATE_DIR"),
            }
            match self.prev_discovery_port.take() {
                Some(v) => std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", v),
                None => std::env::remove_var("INKENTRY_TEST_DISCOVERY_PORT"),
            }
            match self.prev_no_server.take() {
                Some(v) => std::env::set_var("INKENTRY_NO_SERVER", v),
                None => std::env::remove_var("INKENTRY_NO_SERVER"),
            }
        }
    }
}

// The fp32 vector `spawn_loopback_embedder`'s `/index/embed` route returns.
// L2-normalised and 896-dim, so it survives the push's own dimension guard.
pub(super) fn stub_vector() -> Vec<f32> {
    let dim = inkentry_core::embeddings::EMBEDDING_DIM;
    vec![1.0 / (dim as f32).sqrt(); dim]
}

// Start a mocked loopback inference server for `project_id` and point
// auto-discovery at it. `failing_title_marker`, when given, makes the embed
// route 500 for any request whose body contains it, so a single row's embed
// failure can be exercised without failing the rest.
//
// This mutates `INKENTRY_STATE_DIR` and `INKENTRY_NO_SERVER`, which are
// process-global. Every caller must carry
// `#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]`:
// those are the keys the rest of the crate guards these two variables under,
// and serial_test's unnamed key is a *separate* lock, so a bare `#[serial]`
// leaves a caller racing the probe and daemon tests. A concurrent probe then
// reads this test's `server.port` and hits this test's mock, which is exactly
// what breaks the "no embed calls" assertions.
pub(super) async fn spawn_loopback_embedder(
    project_id: &str,
    failing_title_marker: Option<&str>,
) -> LoopbackEmbedder {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    mount_health(&server).await;
    let embed_path = format!("/v1/projects/{project_id}/index/embed");
    if let Some(marker) = failing_title_marker {
        Mock::given(method("POST"))
            .and(path(embed_path.clone()))
            .and(body_string_contains(marker))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(embed_path))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(inkentry_core::embeddings::vec_to_blob(&stub_vector())),
        )
        .mount(&server)
        .await;

    point_discovery_at(server)
}

// The same loopback embedder, but answering with a vector derived from the
// document it was handed rather than one constant vector. Two texts sharing
// words come back close together and unrelated ones do not, which is what lets
// a test assert a real KNN round trip instead of "some blob was written".
//
// Carries the same `#[serial]` requirement as `spawn_loopback_embedder`.
pub(super) async fn spawn_content_embedder(
    project_id: &str,
    failing_title_marker: Option<&str>,
) -> LoopbackEmbedder {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct ByContent;
    impl wiremock::Respond for ByContent {
        fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let content = body["chunks"][0]["content"].as_str().unwrap_or_default();
            ResponseTemplate::new(200).set_body_bytes(inkentry_core::embeddings::vec_to_blob(
                &content_vector(content),
            ))
        }
    }

    let server = MockServer::start().await;
    mount_health(&server).await;
    let embed_path = format!("/v1/projects/{project_id}/index/embed");
    if let Some(marker) = failing_title_marker {
        Mock::given(method("POST"))
            .and(path(embed_path.clone()))
            .and(body_string_contains(marker))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(embed_path))
        .respond_with(ByContent)
        .mount(&server)
        .await;

    point_discovery_at(server)
}

// A unit vector whose direction is the bag of words of `text`: every token is
// hashed to one dimension. Shared tokens are shared direction, so cosine
// distance behaves the way a real embedder's does for the purposes of "does the
// right entry come back first".
pub(super) fn content_vector(text: &str) -> Vec<f32> {
    let dim = inkentry_core::embeddings::EMBEDDING_DIM;
    let mut v = vec![0f32; dim];
    for token in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in token.to_ascii_lowercase().as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        v[(h % dim as u64) as usize] += 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    } else {
        // A zero vector has no direction for a distance metric to compare, so a
        // token-less document still gets a unit one.
        v[0] = 1.0;
    }
    v
}

async fn mount_health(server: &wiremock::MockServer) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "0.9.5",
            "capabilities": ["memory", "index.embed", "search.semantic"],
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "started_by": null,
            "embedding_dim": inkentry_core::embeddings::EMBEDDING_DIM,
        })))
        .mount(server)
        .await;
}

// Through the fixed-port fallback (step 3b), not the `server.port` file: step
// 3a now uses a responder only when the pid recorded beside the port is a live
// `inkentry-server` process reporting the recorded instance id, and a wiremock
// stand-in is neither. The state dir is still redirected at an empty temp dir,
// so nothing here reads the developer's own state.
fn point_discovery_at(server: wiremock::MockServer) -> LoopbackEmbedder {
    let port = server.address().port();
    let state_dir = tempfile::TempDir::new().unwrap();
    let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
    let prev_discovery_port = std::env::var_os("INKENTRY_TEST_DISCOVERY_PORT");
    let prev_no_server = std::env::var_os("INKENTRY_NO_SERVER");
    unsafe {
        std::env::set_var("INKENTRY_STATE_DIR", state_dir.path());
        std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", port.to_string());
        std::env::remove_var("INKENTRY_NO_SERVER");
    }
    LoopbackEmbedder {
        server,
        _state_dir: state_dir,
        prev_state_dir,
        prev_discovery_port,
        prev_no_server,
    }
}

// Open a fresh local memory store in a new tempdir, returning both (the
// tempdir must be kept alive by the caller for the store's lifetime).
pub(super) fn fresh_store() -> (tempfile::TempDir, MemoryStore) {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    (tmp, store)
}
