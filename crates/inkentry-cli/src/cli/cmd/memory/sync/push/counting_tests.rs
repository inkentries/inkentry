use super::super::test_support::register_sqlite_vec;
use super::*;

// Stamping a non-persisted status would exclude the row from `live` on every
// future push, so it could never be retried.
#[tokio::test]
async fn push_local_does_not_stamp_remote_id_for_a_failed_status_item() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 1);
    let ext_a = rows[0].id.to_string();
    let cloud_a = "01890000-0000-7000-8000-0000000000b1";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 1,
            "results": [
                {"status": "failed", "external_id": ext_a, "id": cloud_a},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!((s1.attempted, s1.created, s1.skipped), (1, 0, 0));

    assert_eq!(store.note_id_for_remote_id(cloud_a).unwrap(), None);
    let rows_after = store.rows_for_sync(false).unwrap();
    assert_eq!(rows_after[0].remote_id, None);

    let live_again: Vec<_> = rows_after
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .collect();
    assert_eq!(live_again.len(), 1, "unstamped row must remain retryable");
}

// The aggregate ints and `results[]` are independent wire fields; the summary
// must read `results[].status`.
#[tokio::test]
async fn push_local_reconciles_counts_from_results_not_aggregate_ints() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    let (ext_a, ext_b) = (rows[0].id.to_string(), rows[1].id.to_string());
    let cloud_a = "01890000-0000-7000-8000-0000000000c1";
    let cloud_b = "01890000-0000-7000-8000-0000000000c2";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 0,
            "results": [
                {"status": "created", "external_id": ext_a, "id": cloud_a},
                {"status": "skipped", "external_id": ext_b, "id": cloud_b},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (s1.attempted, s1.created, s1.skipped, s1.failed),
        (2, 1, 1, 0)
    );
    assert_eq!(
        store.note_id_for_remote_id(cloud_a).unwrap(),
        Some(rows[0].id.clone())
    );
    assert_eq!(
        store.note_id_for_remote_id(cloud_b).unwrap(),
        Some(rows[1].id.clone())
    );
}

#[tokio::test]
async fn push_local_partial_failure_reports_the_real_successes() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    let (ext_a, ext_b) = (rows[0].id.to_string(), rows[1].id.to_string());
    let cloud_a = "01890000-0000-7000-8000-0000000000d1";
    let cloud_b = "01890000-0000-7000-8000-0000000000d2";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 1,
            "results": [
                {"status": "created", "external_id": ext_a, "id": cloud_a},
                {"status": "failed", "external_id": ext_b, "id": cloud_b},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (s1.attempted, s1.created, s1.skipped, s1.failed),
        (2, 1, 0, 1),
        "attempted must stay 2 (not read as nothing-to-push) and the \
             genuine success must be visible alongside the failure"
    );
    assert_eq!(
        store.note_id_for_remote_id(cloud_a).unwrap(),
        Some(rows[0].id.clone())
    );
    assert_eq!(store.note_id_for_remote_id(cloud_b).unwrap(), None);
    let rows_after = store.rows_for_sync(false).unwrap();
    let live_again: Vec<_> = rows_after
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .collect();
    assert_eq!(live_again.len(), 1, "failed row must remain retryable");
}

// The command-layer `bail!` (non-zero exit) is covered by
// tests/memory_push_sync_total_failure.rs; this pins only `push_local`'s own
// return value for the all-failed shape.
#[tokio::test]
async fn push_local_total_failure_reports_zero_created_and_skipped() {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    let (ext_a, ext_b) = (rows[0].id.to_string(), rows[1].id.to_string());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 2,
            "results": [
                {"status": "failed", "external_id": ext_a, "id": serde_json::Value::Null},
                {"status": "failed", "external_id": ext_b, "id": serde_json::Value::Null},
            ]
        })))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s1 = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (s1.attempted, s1.created, s1.skipped, s1.failed),
        (2, 0, 0, 2),
        "total failure: attempted > 0 but nothing durably landed"
    );
    let rows_after = store.rows_for_sync(false).unwrap();
    assert!(rows_after.iter().all(|r| r.remote_id.is_none()));
}
