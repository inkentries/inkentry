use super::super::test_support::register_sqlite_vec;
use super::*;

fn note_with_embedding(store: &MemoryStore) -> NoteId {
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    let dim = inkentry_core::embeddings::EMBEDDING_DIM;
    let vec: Vec<f32> = vec![1.0 / (dim as f32).sqrt(); dim];
    let blob = inkentry_core::embeddings::vec_to_blob(&vec);
    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 1);
    store.insert_embedding(&rows[0].id, &blob).unwrap();
    rows[0].id.clone()
}

#[tokio::test]
async fn push_local_attaches_vector_when_server_accepts() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    let id = note_with_embedding(&store);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": id.to_string(), "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    push_local(&store, &client, false, true, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let entry = &json["entries"][0];
    assert_eq!(
        entry["vector"].as_array().map(Vec::len),
        Some(inkentry_core::embeddings::EMBEDDING_DIM),
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
    let id = note_with_embedding(&store);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": id.to_string(), "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(
        !body.contains("vector"),
        "server without the capability must get a text-only push: {body}"
    );
}

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

    let stale_768_blob = inkentry_core::embeddings::vec_to_blob(&vec![1.0f32; 768]);
    let err = store
        .insert_embedding(&rows[0].id, &stale_768_blob)
        .unwrap_err();
    assert!(
        err.to_string().contains("896") && err.to_string().contains("768"),
        "the vec0 FLOAT[896] column must refuse a 768-dim insert outright \
             (this is what makes a wrong-dimension row unreachable via any \
             application write path): {err}"
    );
}
