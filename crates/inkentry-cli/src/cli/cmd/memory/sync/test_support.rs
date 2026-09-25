use crate::storage::MemoryStore;

pub(in crate::cli::cmd::memory) fn register_sqlite_vec() {
    use std::sync::OnceLock;
    // `MemoryStore::open` creates a vec0 table, so this must precede any connection.
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

pub(in crate::cli::cmd::memory) async fn spawn_inkentry_server() -> std::net::SocketAddr {
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
            inkentry_server::EMBED_INTERACTIVE_CAPACITY_HIGH,
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

// Found through real auto-discovery (fixed-port fallback pointed at the mock),
// which is what makes the "embed never reaches the team server_url" tests
// meaningful. Mutates process-global env, so users must be `#[serial]`.
pub(in crate::cli::cmd::memory) struct LoopbackEmbedder {
    pub(in crate::cli::cmd::memory) server: wiremock::MockServer,
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

// L2-normalised and 896-dim, so it survives the push's dimension guard.
pub(in crate::cli::cmd::memory) fn stub_vector() -> Vec<f32> {
    let dim = inkentry_core::embeddings::EMBEDDING_DIM;
    vec![1.0 / (dim as f32).sqrt(); dim]
}

// `failing_title_marker` makes the embed route 500 for any request body
// containing it, so one row can fail without failing the rest.
//
// Callers must carry `#[serial_test::serial(inkentry_no_server_env,
// server_state_dir_env)]`: a bare `#[serial]` uses a separate lock and races
// the probe and daemon tests that guard these env vars under those keys.
pub(in crate::cli::cmd::memory) async fn spawn_loopback_embedder(
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

// Answers with a vector derived from the document, so texts sharing words land
// close together and a test can assert a real KNN round trip. Same `#[serial]`
// requirement as `spawn_loopback_embedder`.
pub(in crate::cli::cmd::memory) async fn spawn_content_embedder(
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

// Bag-of-words unit vector: each token hashes to one dimension.
pub(in crate::cli::cmd::memory) fn content_vector(text: &str) -> Vec<f32> {
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
        // A zero vector has no direction to compare.
        v[0] = 1.0;
    }
    v
}

pub(in crate::cli::cmd::memory) async fn mount_health(server: &wiremock::MockServer) {
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

// Uses the fixed-port fallback, not the `server.port` file: that path accepts
// only a live `inkentry-server` pid, which a wiremock stand-in is not. The
// state dir is redirected to an empty temp dir so the developer's is never read.
pub(in crate::cli::cmd::memory) fn point_discovery_at(
    server: wiremock::MockServer,
) -> LoopbackEmbedder {
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

// The caller must keep the tempdir alive for the store's lifetime.
pub(in crate::cli::cmd::memory) fn fresh_store() -> (tempfile::TempDir, MemoryStore) {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    (tmp, store)
}
