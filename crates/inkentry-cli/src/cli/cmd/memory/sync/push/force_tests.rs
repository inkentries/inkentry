// `plumbing push --force` recovery path (ADR-092): the force push re-offers
// every active entry, hands each already-synced entry's existing `remote_id`
// back to the server as the ingest `id`, and reports the outcomes as
// created/skipped rather than already_synced. The normal (non-force) push is
// unchanged: it pushes only unstamped rows and never sends an `id`.

use super::super::test_support::register_sqlite_vec;
use super::*;

use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// Echoes every received entry back with a fixed status, so a force push against
// a reset server (`created`) or a healthy one (`skipped`) can be modelled while
// echoing the per-entry ids a real 207 would carry.
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
                        // Echo the supplied id back when present (a --force
                        // restore), otherwise a distinct minted-style id.
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

// Seed two notes and mark them already-synced by stamping a `remote_id` on each
// (as a prior sync would have). Returns the store and the two remote ids.
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

// Against a server that lost its database, `--force` re-offers every already-
// synced entry (a normal push would skip them as attempted:0), hands each
// entry's own prior `remote_id` back as the ingest `id`, and reports them as
// created rather than already_synced.
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

    // Baseline: a normal push has nothing to do — both rows are already synced.
    let normal = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(
        (normal.attempted, normal.already_synced),
        (0, 2),
        "the normal push must treat both stamped rows as already-synced"
    );

    // Force: re-offer both, report them as created, and never as already_synced.
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

    // The force push (the only request made — the normal push sent none) must
    // carry each entry's existing remote_id as the request `id`.
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

// Against a still-healthy server the same `--force` re-push is idempotent: every
// entry comes back skipped (the server already holds it), and the report counts
// them as skipped, never already_synced — no duplicates.
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

// The normal (non-force) push is unchanged: it pushes only unstamped rows and
// its request never carries an `id` field (the server mints).
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
