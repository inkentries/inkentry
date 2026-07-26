//! Pushed-vector fast-path tests for [`super::push_local`].

use super::super::test_support::register_sqlite_vec;
use super::*;

// ── pushed-vector fast path ─────────────────────────────────────────────
// A note with a local fp32/896 embedding carries that vector (+ model tag
// + precision "fp32") to a server advertising `accepts_pushed_vectors`, so
// the server stores it as-is; against a server without the capability the
// same note is pushed text-only even though the vector is available. This
// exercises the full `push_local` wiring: it reads the local embedding and
// consults the gate, which the `maybe_attach_vector` unit test cannot.

/// Insert an active note plus a valid L2-normalised fp32/896 embedding,
/// returning its local id + external uuid.
fn note_with_embedding(store: &MemoryStore) -> (i64, String) {
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    let dim = spelunk_core::embeddings::EMBEDDING_DIM;
    let vec: Vec<f32> = vec![1.0 / (dim as f32).sqrt(); dim];
    let blob = spelunk_core::embeddings::vec_to_blob(&vec);
    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 1);
    store.insert_embedding(rows[0].local_id, &blob).unwrap();
    (rows[0].local_id, rows[0].uuid.clone())
}

#[tokio::test]
async fn push_local_attaches_vector_when_server_accepts() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    let (_id, uuid) = note_with_embedding(&store);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    // accepts_pushed_vectors = true → the fp32/896 vector reaches the wire.
    push_local(&store, &client, false, true).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let entry = &json["entries"][0];
    assert_eq!(
        entry["vector"].as_array().map(Vec::len),
        Some(spelunk_core::embeddings::EMBEDDING_DIM),
        "server that accepts vectors must receive the 896-dim vector: {entry}"
    );
    assert_eq!(entry["vector_model"], "F2LLM-v2-330M");
    assert_eq!(entry["vector_precision"], "fp32");
}

#[tokio::test]
async fn push_local_stays_text_only_when_server_declines() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    let (_id, uuid) = note_with_embedding(&store);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    // accepts_pushed_vectors = false → text-only, despite a local vector.
    push_local(&store, &client, false, false).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(
        !body.contains("vector"),
        "server without the capability must get a text-only push: {body}"
    );
}

/// A note queued for push with NO `note_embeddings` row at all (never
/// embedded locally, or embedding failed) must fall back to a text-only
/// push for that row — not crash, and not send an empty/malformed
/// `vector` field — even though the server accepts pushed vectors.
#[tokio::test]
async fn push_local_falls_back_to_text_only_when_local_embedding_missing() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    // Deliberately no `insert_embedding` call — this note has never been
    // embedded.
    store
        .add_note("decision", "Unembedded", "first", &[], &[], None, None)
        .unwrap();
    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 1);
    let uuid = rows[0].uuid.clone();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    // accepts_pushed_vectors = true, but no local embedding exists.
    let summary = push_local(&store, &client, false, true).await.unwrap();
    assert_eq!(
        (summary.attempted, summary.created, summary.failed),
        (1, 1, 0)
    );

    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(
        !body.contains("vector"),
        "a note with no local embedding must fall back to text-only, \
             not error or send a malformed vector: {body}"
    );
}

/// `note_embeddings` is a `vec0` virtual table with a `FLOAT[896]` column
/// (migration `004_memory.sql`) — sqlite-vec enforces that exact
/// dimension AT INSERT TIME, for every write path (there is only one:
/// `insert_embedding`). So a "leftover pre-896 768-dim row" — unlike the
/// code-chunk `embeddings` table, which DID have a legacy 768-dim era
/// with an explicit recreate-on-open migration in `db.rs` — can never
/// actually be written for memory notes: there was never a 768-dim
/// memory-embedding vintage to migrate from, and the store itself
/// refuses the write. Confirmed here rather than assumed, since it is
/// exactly the scenario `push_local`'s dimension guard names in its
/// comment.
#[tokio::test]
async fn insert_embedding_rejects_wrong_dimension_vector() {
    use tempfile::TempDir;

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    let rows = store.rows_for_sync(false).unwrap();

    let stale_768_blob = spelunk_core::embeddings::vec_to_blob(&vec![1.0f32; 768]);
    let err = store
        .insert_embedding(rows[0].local_id, &stale_768_blob)
        .unwrap_err();
    assert!(
        err.to_string().contains("896") && err.to_string().contains("768"),
        "the vec0 FLOAT[896] column must refuse a 768-dim insert outright \
             (this is what makes a wrong-dimension row unreachable via any \
             application write path): {err}"
    );
}

/// Since a wrong-dimension row can never be *written* (previous test),
/// the only way `push_local`'s guard (`blob_to_vec` decode + a length
/// check against `EMBEDDING_DIM` — the same two building blocks
/// exercised inline at `sync.rs`'s vector-resolution site) could ever
/// see a wrong-length vector is an on-disk blob corrupted or torn
/// independently of any insert (disk fault, a crash mid-write). This
/// pins that composed guard logic directly: it must never panic on a
/// short/truncated/empty blob, and must always filter such a decode out
/// rather than accept a spurious length or garbage-padded 896 vector.
#[test]
fn dim_guard_logic_rejects_short_truncated_and_empty_blobs() {
    use spelunk_core::embeddings::{EMBEDDING_DIM, blob_to_vec, vec_to_blob};

    let guarded = |blob: &[u8]| -> Option<Vec<f32>> {
        Some(blob_to_vec(blob)).filter(|v| v.len() == EMBEDDING_DIM)
    };

    // A stale 768-float blob (wrong dimension, but cleanly decodable).
    let stale_768 = vec_to_blob(&vec![1.0f32; 768]);
    assert!(
        guarded(&stale_768).is_none(),
        "a 768-float blob must be filtered out, not accepted"
    );

    // A torn write: a valid 896-dim blob with its last few bytes cut off.
    let full = vec_to_blob(&vec![1.0f32; EMBEDDING_DIM]);
    let truncated = &full[..full.len() - 10];
    assert_ne!(
        blob_to_vec(truncated).len(),
        EMBEDDING_DIM,
        "sanity: a truncated blob must decode to a non-896 length"
    );
    assert!(
        guarded(truncated).is_none(),
        "a truncated blob must never pass the dimension guard"
    );

    // A zero-length blob (e.g. corrupted read).
    assert!(
        guarded(&[]).is_none(),
        "an empty blob must be filtered out, not treated as a valid vector"
    );
}
