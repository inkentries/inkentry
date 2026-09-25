use super::super::test_support::register_sqlite_vec;
use super::*;

use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// Echoes each entry back with a fixed status and the per-entry ids a real 207
// carries.
struct EchoStatus(&'static str);
impl Respond for EchoStatus {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        let results: Vec<serde_json::Value> = body["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| {
                        let ext = e["external_id"].as_str().unwrap_or_default();
                        // A --force restore supplies the id; echo it back.
                        let id = e["id"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("cloud-{ext}"));
                        serde_json::json!({
                            "status": self.0, "external_id": ext, "id": id,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (created, skipped) = if self.0 == "created" {
            (results.len(), 0)
        } else {
            (0, results.len())
        };
        ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": created, "skipped": skipped, "failed": 0, "results": results
        }))
    }
}

fn store_with_two_synced_rows(tmp: &TempDir) -> (MemoryStore, String, String) {
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();
    let rows = store.rows_for_sync(false).unwrap();
    let remote_a = "01890000-0000-7000-8000-0000000000a1".to_string();
    let remote_b = "01890000-0000-7000-8000-0000000000a2".to_string();
    store.set_remote_id(&rows[0].id, &remote_a).unwrap();
    store.set_remote_id(&rows[1].id, &remote_b).unwrap();
    (store, remote_a, remote_b)
}

#[tokio::test]
async fn force_reoffers_synced_rows_and_sends_their_remote_id_as_id() {
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let (store, remote_a, remote_b) = store_with_two_synced_rows(&tmp);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(EchoStatus("created"))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let normal = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (normal.attempted, normal.already_synced),
        (0, 2),
        "the normal push must treat both stamped rows as already-synced"
    );

    let forced = push_local_oneway(&store, &client, false, false, true, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (
            forced.attempted,
            forced.created,
            forced.skipped,
            forced.already_synced
        ),
        (2, 2, 0, 0),
        "force re-offers every active entry and counts them as created against a reset server"
    );

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "only the force push makes a request");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let ids: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().expect("force must send an id"))
        .collect();
    assert!(
        ids.contains(&remote_a.as_str()) && ids.contains(&remote_b.as_str()),
        "force must hand each entry's own prior remote_id back as the id: {body}"
    );
}

#[tokio::test]
async fn force_against_a_healthy_server_reports_skipped_not_already_synced() {
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let (store, _a, _b) = store_with_two_synced_rows(&tmp);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(EchoStatus("skipped"))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let forced = push_local_oneway(&store, &client, false, false, true, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (
            forced.attempted,
            forced.created,
            forced.skipped,
            forced.already_synced
        ),
        (2, 0, 2, 0),
        "against a healthy server force re-push is all skipped, reported as skipped not already_synced"
    );
}

#[tokio::test]
async fn normal_push_never_sends_an_id_field() {
    register_sqlite_vec();
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(&tmp.path().join("memory.db")).unwrap();
    store
        .add_note("decision", "Fresh", "never synced", &[], &[], None, None)
        .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(EchoStatus("created"))
        .mount(&server)
        .await;
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let s = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!((s.attempted, s.created), (1, 1));

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(
        body["entries"][0].get("id").is_none(),
        "a normal push must omit the id field entirely: {body}"
    );
}
