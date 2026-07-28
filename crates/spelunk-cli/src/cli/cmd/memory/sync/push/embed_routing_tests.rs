//! Where the pre-batch local embed is allowed to go, and what happens when it
//! cannot run at all.
//!
//! The load-bearing invariant here: the embed must reach the loopback embedder
//! and never the configured team `server_url`. Routing it there would re-create
//! the exact server-side re-embedding the repair exists to remove.

use super::super::test_support::{fresh_store, spawn_loopback_embedder};
use super::*;
use crate::config::{Config, SyncMode};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn add_note(store: &MemoryStore, title: &str) -> String {
    store
        .add_note("decision", title, "body", &[], &[], None, None)
        .unwrap();
    let rows = store.rows_for_sync(true).unwrap();
    rows.iter()
        .find(|r| r.title == title)
        .expect("note added")
        .uuid
        .clone()
}

async fn mount_batch_created(server: &MockServer, uuid: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
        })))
        .mount(server)
        .await;
}

fn paths(reqs: &[wiremock::Request]) -> Vec<String> {
    reqs.iter().map(|r| r.url.path().to_string()).collect()
}

// ── the never-route-to-server_url invariant ─────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn embed_traffic_goes_to_loopback_and_never_to_the_team_server_url() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let uuid = add_note(&store, "Unembedded");

    let team = MockServer::start().await;
    mount_batch_created(&team, &uuid).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    // A real team server_url, and a loopback embedder discovered alongside it.
    let cfg = Config {
        server_url: Some(team.uri()),
        project_id: Some("proj".to_string()),
        mode: None,
        ..Default::default()
    };
    assert_eq!(cfg.resolve_mode(), SyncMode::LocalFirst);
    let summary = push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert_eq!(summary.embedded_locally, 1);
    let team_paths = paths(&team.received_requests().await.unwrap());
    assert!(
        !team_paths.iter().any(|p| p.contains("embed")),
        "no embed request may reach the configured team server: {team_paths:?}"
    );
    let loopback_paths = paths(&loopback.server.received_requests().await.unwrap());
    assert_eq!(
        loopback_paths
            .iter()
            .filter(|p| p.ends_with("/index/embed"))
            .count(),
        1,
        "the embed must land on loopback instead: {loopback_paths:?}"
    );
    drop(loopback);
}

#[tokio::test]
#[serial_test::serial]
async fn a_push_speaks_only_to_loopback_and_the_configured_team_server() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let uuid = add_note(&store, "Unembedded");

    let team = MockServer::start().await;
    mount_batch_created(&team, &uuid).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = Config {
        server_url: Some(team.uri()),
        project_id: Some("proj".to_string()),
        mode: None,
        ..Default::default()
    };
    push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    for p in paths(&loopback.server.received_requests().await.unwrap()) {
        assert!(
            p == "/v1/health" || p == "/v1/projects/proj/index/embed",
            "unexpected loopback destination: {p}"
        );
    }
    for p in paths(&team.received_requests().await.unwrap()) {
        assert!(
            p == "/v1/projects/proj/memory/batch",
            "unexpected team-server destination: {p}"
        );
    }
    drop(loopback);
}

// ── no local embedder: degrade, never refuse ────────────────────────────────

// `mode = "offline"` is the deterministic form of "no local embedder is
// reachable": `get_inference_tier` short-circuits before any probe, so the
// repair resolves no client at all, exactly as it would on a machine with no
// `spelunk server` running.
fn no_embedder_cfg(server_url: String) -> Config {
    Config {
        server_url: Some(server_url),
        project_id: Some("proj".to_string()),
        mode: Some(SyncMode::Offline),
        ..Default::default()
    }
}

#[tokio::test]
async fn push_completes_text_only_when_no_local_embedder_is_available() {
    let (tmp, store) = fresh_store();
    let uuid = add_note(&store, "Unembedded");

    let team = MockServer::start().await;
    mount_batch_created(&team, &uuid).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = no_embedder_cfg(team.uri());
    let summary = push_local(
        &store,
        &client,
        false,
        true,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    // Same success shape as before the repair existed: nothing refused.
    assert_eq!(
        (summary.attempted, summary.created, summary.failed),
        (1, 1, 0)
    );
    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (0, 1)
    );
    let reqs = team.received_requests().await.unwrap();
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(!body.contains("vector"), "must ship text-only: {body}");
}

#[tokio::test]
async fn no_local_embedder_sends_no_embed_request_to_the_team_server() {
    let (tmp, store) = fresh_store();
    let uuid = add_note(&store, "Unembedded");

    let team = MockServer::start().await;
    mount_batch_created(&team, &uuid).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = no_embedder_cfg(team.uri());
    push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    let team_paths = paths(&team.received_requests().await.unwrap());
    assert!(
        !team_paths.iter().any(|p| p.contains("embed")),
        "the absent local embedder must never degrade into a remote embed: {team_paths:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn one_row_embed_failure_leaves_the_rest_of_the_push_intact() {
    let loopback = spawn_loopback_embedder("proj", Some("Poison")).await;
    let (tmp, store) = fresh_store();
    let bad = add_note(&store, "Poison");
    let good = add_note(&store, "Fine");

    let team = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 2, "skipped": 0, "failed": 0,
            "results": [
                {"status": "created", "external_id": bad, "id": "cloud-1"},
                {"status": "created", "external_id": good, "id": "cloud-2"},
            ]
        })))
        .mount(&team)
        .await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = Config {
        server_url: Some(team.uri()),
        project_id: Some("proj".to_string()),
        mode: None,
        ..Default::default()
    };
    let summary = push_local(
        &store,
        &client,
        false,
        false,
        &LocalEmbedPolicy::for_push(&cfg, &tmp.path().join("memory.db")),
    )
    .await
    .unwrap();

    assert_eq!(
        (summary.attempted, summary.created, summary.failed),
        (2, 2, 0),
        "a failed embed must not abort or fail the push"
    );
    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (1, 1)
    );
    let rows = store.rows_for_sync(false).unwrap();
    for r in &rows {
        let embedded = store.get_embedding(r.local_id).unwrap().is_some();
        assert_eq!(
            embedded,
            r.title == "Fine",
            "only the row whose embed succeeded may be embedded: {}",
            r.title
        );
    }
    drop(loopback);
}

// ── applicability ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn cloud_first_with_server_url_skips_the_repair_entirely() {
    let loopback = spawn_loopback_embedder("proj", None).await;
    let (tmp, store) = fresh_store();
    let uuid = add_note(&store, "Unembedded");

    let team = MockServer::start().await;
    mount_batch_created(&team, &uuid).await;
    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();

    let cfg = Config {
        server_url: Some(team.uri()),
        project_id: Some("proj".to_string()),
        mode: Some(SyncMode::CloudFirst),
        ..Default::default()
    };
    let mem_path = tmp.path().join("memory.db");
    assert!(
        matches!(
            LocalEmbedPolicy::for_push(&cfg, &mem_path),
            LocalEmbedPolicy::Skip
        ),
        "memory.db is not the store of record here, so there is nothing to repair"
    );

    let summary = push_local(
        &store,
        &client,
        false,
        true,
        &LocalEmbedPolicy::for_push(&cfg, &mem_path),
    )
    .await
    .unwrap();

    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (0, 0),
        "a skipped repair must not report counts, or warn"
    );
    let rows = store.rows_for_sync(false).unwrap();
    assert!(store.get_embedding(rows[0].local_id).unwrap().is_none());
    let loopback_paths = paths(&loopback.server.received_requests().await.unwrap());
    assert!(
        !loopback_paths.iter().any(|p| p.ends_with("/index/embed")),
        "no embed call may be made: {loopback_paths:?}"
    );
    let reqs = team.received_requests().await.unwrap();
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(!body.contains("vector"), "wire payload unchanged: {body}");
    drop(loopback);
}

#[test]
fn cloud_first_without_server_url_still_repairs() {
    // `open_memory_backend` only relocates the store of record when a
    // `server_url` actually exists, so memory.db is still local truth here.
    // `memory reindex` applies under exactly this condition too.
    let cfg = Config {
        server_url: None,
        mode: Some(SyncMode::CloudFirst),
        ..Default::default()
    };
    assert!(matches!(
        LocalEmbedPolicy::for_push(&cfg, std::path::Path::new("/tmp/p/memory.db")),
        LocalEmbedPolicy::Repair { .. }
    ));
}

// ── reporting ───────────────────────────────────────────────────────────────

#[test]
fn wrong_dimension_stored_vector_counts_as_missing() {
    // `note_embeddings` is a vec0 `FLOAT[896]` column that refuses a
    // wrong-dimension insert outright (see `vector_tests.rs`), so this state is
    // only reachable through a blob torn independently of any write. Pinned on
    // the shared predicate both the repair pass and the batch build consult:
    // "not exactly 896 floats" must mean "unembedded", so the row is re-embedded
    // and the corrected vector is what ships.
    use spelunk_core::embeddings::{EMBEDDING_DIM, vec_to_blob};

    assert!(usable_vector(Some(vec_to_blob(&vec![1.0f32; 768]))).is_none());
    assert!(usable_vector(Some(Vec::new())).is_none());
    assert!(usable_vector(None).is_none());
    // A torn write: a valid 896-dim blob with its tail cut off. Must filter
    // out rather than panic or accept a garbage-padded length.
    let full = vec_to_blob(&vec![1.0f32; EMBEDDING_DIM]);
    assert!(usable_vector(Some(full[..full.len() - 10].to_vec())).is_none());
    assert_eq!(
        usable_vector(Some(vec_to_blob(&vec![1.0f32; EMBEDDING_DIM]))).map(|v| v.len()),
        Some(EMBEDDING_DIM)
    );
}

#[test]
fn the_unembedded_warning_names_the_count_and_the_remedy() {
    let msg = unembedded_warning(3);
    assert!(msg.contains('3'), "must name the count: {msg}");
    assert!(
        msg.contains("spelunk memory reindex"),
        "must name the remedy: {msg}"
    );
    assert!(
        msg.contains("memory search"),
        "must say what is actually broken: {msg}"
    );
    assert!(
        unembedded_warning(1).contains("1 entry"),
        "singular must not read as '1 entries'"
    );
}

#[test]
fn the_summary_clause_reports_local_embedding_separately_from_push_results() {
    let summary = |embedded, missing| PushSummary {
        attempted: 5,
        created: 5,
        skipped: 0,
        failed: 0,
        already_synced: 0,
        interrupted: None,
        embedded_locally: embedded,
        without_local_vector: missing,
    };
    assert_eq!(
        local_embed_summary(&summary(0, 0)),
        "",
        "an unaffected push must read exactly as it did before"
    );
    assert_eq!(local_embed_summary(&summary(2, 0)), " Embedded 2 locally.");
    let both = local_embed_summary(&summary(2, 3));
    assert!(
        both.contains("Embedded 2 locally") && both.contains('3'),
        "{both}"
    );
}
