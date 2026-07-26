//! Shared test fixtures for the `sync` module's submodule test suites
//! ([`super::push`], [`super::pull`], [`super::round`]).

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

/// Spin up a real `spelunk-server` axum router (the production router) on
/// an ephemeral loopback port, serving the team-hosting
/// `/v1/projects/*/memory*` routes this test's `CloudSyncClient`s talk to.
pub(super) async fn spawn_spelunk_server() -> std::net::SocketAddr {
    register_sqlite_vec();
    let db_dir = tempfile::TempDir::new().unwrap();
    let db = spelunk_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
        .unwrap();
    let instance_id = db.get_or_create_instance_id().unwrap();
    let state = spelunk_server::AppState {
        db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
        auth: std::sync::Arc::new(spelunk_server::auth::ApiKeyAuth::new(None)),
        conflict_threshold: spelunk_server::default_conflict_threshold(),
        embedder: spelunk_server::EmbedderSlot::disabled(),
        embed_admission: spelunk_server::EmbedAdmission::new(
            spelunk_server::EMBED_QUEUE_CAPACITY,
            spelunk_server::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: std::sync::Arc::new(spelunk_server::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        relay: spelunk_server::relay::RelayRegistry::new(),
    };
    let app = spelunk_server::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// Open a fresh local memory store in a new tempdir, returning both (the
/// tempdir must be kept alive by the caller for the store's lifetime).
pub(super) fn fresh_store() -> (tempfile::TempDir, MemoryStore) {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    (tmp, store)
}
