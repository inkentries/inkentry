// The pull half of the local-embedding repair.
//
// A pull used to write every entry through `apply_remote_note`, whose INSERT
// has no vector column, so a pulled entry landed with no local vector and was
// invisible to semantic `memory search` with nothing telling the user. The push
// half already solved this for the rows it owns and deliberately excluded the
// synced ones (`already_synced_rows_are_left_unembedded`), which is why no
// later sync repaired them.
//
// These tests assert on identity (titles, retrieved content, the vector bytes
// on disk) and never on how many rows moved: a count can pass while the write went
// to a different store.

use super::local_embed::{LocalEmbedPolicy, pending_embedding_warning, pull_embed_summary};
use super::pull::{PullSummary, pull_and_apply, pull_and_apply_since};
use super::push::push_local;
use super::round::sync_round;
use super::test_support::{content_vector, fresh_store, spawn_content_embedder};
use crate::config::{Config, SyncMode};
use crate::storage::{CloudSyncClient, MemoryStore, NoteId};

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

// Deterministic, lexically-increasing ids so `since_id` cursors compare the way
// real UUIDv7 cloud ids do.
fn remote_id(i: usize) -> String {
    format!("01890000-0000-7000-8000-{i:012x}")
}

// One remote entry: (cloud id, title, body).
type Remote = (String, String, String);

fn entry(i: usize, title: &str, body: &str) -> Remote {
    (remote_id(i), title.to_string(), body.to_string())
}

fn entries_json(entries: &[Remote]) -> serde_json::Value {
    let entries: Vec<_> = entries
        .iter()
        .map(|(id, title, body)| {
            serde_json::json!({
                "id": id,
                "kind": "decision",
                "title": title,
                "body": body,
                "created_at": "2026-06-19T01:00:00Z",
            })
        })
        .collect();
    serde_json::json!({ "entries": entries, "count": entries.len() })
}

// Mount one `/memory/since` page per slice, each matched by the exact
// `since_id` it must be requested with, and answerable `times` times.
async fn mount_pages_times(server: &MockServer, pages: &[Vec<Remote>], times: Option<u64>) {
    let mut cursor = NIL_UUID.to_string();
    for page in pages {
        let mock = Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", cursor.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(page)));
        match times {
            Some(n) => mock.expect(n).mount(server).await,
            None => mock.mount(server).await,
        }
        if let Some((id, _, _)) = page.last() {
            cursor = id.clone();
        }
    }
}

async fn mount_pages(server: &MockServer, pages: &[Vec<Remote>]) {
    mount_pages_times(server, pages, None).await;
}

// A config whose local embedder is whatever auto-discovery finds, i.e. the
// mock the caller spawned. `project_id` must match the embedder's mounted path.
fn discovering_cfg() -> Config {
    Config {
        project_id: Some("proj".to_string()),
        mode: None,
        ..Default::default()
    }
}

// `mode = "offline"` is the deterministic form of "no local embedder is
// reachable": `get_inference_tier` short-circuits before any probe, so the pass
// resolves no client at all, exactly as on a machine with no server running.
// Same fixture the push side uses for its degrade-gracefully tests.
fn no_embedder_cfg() -> Config {
    Config {
        project_id: Some("proj".to_string()),
        mode: Some(SyncMode::Offline),
        ..Default::default()
    }
}

fn policy<'a>(cfg: &'a Config, tmp: &tempfile::TempDir) -> LocalEmbedPolicy<'a> {
    LocalEmbedPolicy::resolve(cfg, &tmp.path().join("memory.db"))
}

fn id_of(store: &MemoryStore, title: &str) -> NoteId {
    store
        .rows_for_sync(true)
        .unwrap()
        .into_iter()
        .find(|r| r.title == title)
        .unwrap_or_else(|| panic!("no local row titled {title:?}"))
        .id
}

fn vector_of(store: &MemoryStore, title: &str) -> Option<Vec<f32>> {
    store
        .get_embedding(&id_of(store, title))
        .unwrap()
        .map(|b| inkentry_core::embeddings::blob_to_vec(&b))
        .filter(|v| v.len() == inkentry_core::embeddings::EMBEDDING_DIM)
}

// The nearest entry to `query`, by the store's own KNN over what is actually on
// disk. `query` is embedded through the same function the mock embedder uses,
// so this is a genuine round trip and not a re-assertion of the write.
fn nearest_title(store: &MemoryStore, query: &str) -> Option<String> {
    let blob = inkentry_core::embeddings::vec_to_blob(&content_vector(query));
    store
        .search(&blob, 5, None)
        .unwrap()
        .first()
        .map(|n| n.title.clone())
}

fn embed_bodies(reqs: &[wiremock::Request]) -> Vec<String> {
    reqs.iter()
        .filter(|r| r.url.path().ends_with("/index/embed"))
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .collect()
}

// ── 1. every pulled entry lands with a usable local vector ──────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn a_pull_leaves_every_entry_it_inserted_with_a_usable_local_vector() {
    let loopback = spawn_content_embedder("proj", None).await;
    let server = MockServer::start().await;
    mount_pages(
        &server,
        &[vec![
            entry(
                0,
                "Postgres partitioning",
                "range partitions on ingest date",
            ),
            entry(1, "Retry budget", "exponential backoff with jitter"),
            entry(2, "Wire format", "little endian fp32 vectors"),
        ]],
    )
    .await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    let summary = pull_and_apply_since(&store, &client, None, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!(summary.applied, 3);
    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (3, 0)
    );
    for title in ["Postgres partitioning", "Retry budget", "Wire format"] {
        assert!(
            vector_of(&store, title).is_some(),
            "{title} was pulled without a usable local vector"
        );
    }
    drop(loopback);
}

// ── 2. and is retrievable by a real local semantic search ───────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn pulled_entries_come_back_from_a_local_knn_search_on_their_own_words() {
    let loopback = spawn_content_embedder("proj", None).await;
    let server = MockServer::start().await;
    mount_pages(
        &server,
        &[vec![
            entry(
                0,
                "Postgres partitioning",
                "range partitions on ingest date",
            ),
            entry(1, "Retry budget", "exponential backoff with jitter"),
            entry(2, "Wire format", "little endian fp32 vectors"),
        ]],
    )
    .await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    pull_and_apply_since(&store, &client, None, &policy(&cfg, &tmp))
        .await
        .unwrap();

    // Each entry must win the search drawn from its own title AND body, and
    // lose the other two: a placeholder vector shared by every row would tie.
    for (query, expected) in [
        (
            "Postgres partitioning range ingest date",
            "Postgres partitioning",
        ),
        ("Retry budget exponential backoff jitter", "Retry budget"),
        ("Wire format little endian fp32 vectors", "Wire format"),
    ] {
        assert_eq!(
            nearest_title(&store, query).as_deref(),
            Some(expected),
            "semantic search for {query:?} did not surface the pulled entry"
        );
    }
    drop(loopback);
}

// ── 3. no embedder now: text-only, then caught up by the next pull ──────────

// "Later" is the next pull of any kind (`inkentry sync`'s pull passes or
// `inkentry plumbing pull`) and it does not depend on that pull returning the
// entry again, or returning anything at all. The second pull below answers with
// an EMPTY page and still embeds the row the first pull had to leave behind.
#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn an_entry_pulled_with_no_embedder_is_embedded_by_the_next_pull() {
    let server = MockServer::start().await;
    mount_pages(
        &server,
        &[vec![entry(
            0,
            "Cold start",
            "the embedder was still loading",
        )]],
    )
    .await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();

    let offline = no_embedder_cfg();
    let first = pull_and_apply_since(&store, &client, None, &policy(&offline, &tmp))
        .await
        .unwrap();
    assert_eq!(first.applied, 1, "the pull itself must still succeed");
    assert_eq!((first.embedded_locally, first.without_local_vector), (0, 1));
    assert!(vector_of(&store, "Cold start").is_none());

    // The next pull, now with an embedder up. Its own page is empty: nothing
    // new comes back, and the catch-up must happen anyway.
    let loopback = spawn_content_embedder("proj", None).await;
    let empty = MockServer::start().await;
    mount_pages(&empty, &[vec![]]).await;
    let client2 = CloudSyncClient::new(&empty.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    let second = pull_and_apply_since(&store, &client2, None, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!(second.applied, 0, "the catch-up pull returned nothing new");
    assert_eq!(
        (second.embedded_locally, second.without_local_vector),
        (1, 0)
    );
    assert_eq!(
        nearest_title(&store, "Cold start the embedder was still loading").as_deref(),
        Some("Cold start"),
        "the caught-up entry must be semantically retrievable"
    );
    drop(loopback);
}

// ── 4. a re-run does not re-embed an already-embedded row ───────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn re_pulling_applied_rows_does_not_re_embed_the_ones_that_have_a_vector() {
    let loopback = spawn_content_embedder("proj", None).await;
    let server = MockServer::start().await;
    mount_pages_times(
        &server,
        &[vec![entry(
            0,
            "Idempotent",
            "applying twice changes nothing",
        )]],
        Some(2),
    )
    .await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    let first = pull_and_apply_since(&store, &client, None, &policy(&cfg, &tmp))
        .await
        .unwrap();
    assert_eq!((first.applied, first.embedded_locally), (1, 1));
    let vector_after_first = vector_of(&store, "Idempotent").unwrap();
    let embeds_after_first =
        embed_bodies(&loopback.server.received_requests().await.unwrap()).len();

    let second = pull_and_apply_since(&store, &client, None, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!((second.applied, second.embedded_locally), (0, 0));
    assert_eq!(
        embed_bodies(&loopback.server.received_requests().await.unwrap()).len(),
        embeds_after_first,
        "an already-embedded row must not be sent to the embedder again"
    );
    assert_eq!(
        vector_of(&store, "Idempotent").unwrap(),
        vector_after_first,
        "the stored vector must be the same bytes, not a rewrite"
    );
    drop(loopback);
}

// ── 5. every page, not only the first ───────────────────────────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn a_multi_page_pull_embeds_the_later_pages_too() {
    let loopback = spawn_content_embedder("proj", None).await;
    let server = MockServer::start().await;
    // A first page at the request limit forces a follow-up request; the entries
    // worth naming live on the second page.
    let limit = CloudSyncClient::MEMORY_SINCE_PULL_LIMIT as usize;
    let page1: Vec<Remote> = (0..limit)
        .map(|i| entry(i, &format!("Filler {i}"), "page one padding"))
        .collect();
    let page2 = vec![
        entry(limit, "Second page anchor", "quorum reads across replicas"),
        entry(limit + 1, "Second page tail", "vacuum thresholds per table"),
    ];
    mount_pages(&server, &[page1, page2]).await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    let summary = pull_and_apply_since(&store, &client, None, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!(summary.applied, limit + 2);
    assert_eq!(summary.without_local_vector, 0);
    assert_eq!(
        nearest_title(&store, "Second page anchor quorum reads across replicas").as_deref(),
        Some("Second page anchor"),
        "an entry from the second page must be searchable, not only the first page's"
    );
    assert!(vector_of(&store, "Second page tail").is_some());
    drop(loopback);
}

// ── 6. sync_round's two pull passes embed a row once ────────────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn sync_rounds_second_pull_pass_does_not_re_embed_what_the_first_embedded() {
    let loopback = spawn_content_embedder("proj", None).await;
    let team = MockServer::start().await;
    // Both passes reuse the same pre-round cursor, so the same page comes back
    // twice; the entry is applied once and must be embedded once.
    let (tmp, store) = fresh_store();
    // An already-synced, already-embedded local row: it gives the store a
    // `MAX(remote_id)` cursor, which is what puts `sync_round` on its
    // established-client path (pull, push, pull) rather than the first-sync one.
    store
        .add_note(
            "decision",
            "Seeded",
            "already synced and embedded",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();
    let seeded = id_of(&store, "Seeded");
    store.set_remote_id(&seeded, &remote_id(0)).unwrap();
    store
        .insert_embedding(
            &seeded,
            &inkentry_core::embeddings::vec_to_blob(&content_vector(
                "title: Seeded | text: already synced and embedded",
            )),
        )
        .unwrap();

    let mut cursor = remote_id(0);
    for _ in 0..2 {
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", cursor.clone()))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(entries_json(&[entry(
                    1,
                    "Teammate entry",
                    "sharded counters under contention",
                )])),
            )
            .mount(&team)
            .await;
        cursor = remote_id(1);
    }
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 0, "results": []
        })))
        .mount(&team)
        .await;

    let client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    let outcome = sync_round(&store, &client, false, false, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!(outcome.pulled.applied, 1, "applied once across both passes");
    assert_eq!(
        (
            outcome.pulled.embedded_locally,
            outcome.pulled.without_local_vector
        ),
        (1, 0),
        "the second pass must find the row already vectored, not embed it again"
    );
    let docs = embed_bodies(&loopback.server.received_requests().await.unwrap());
    let touching_teammate = docs.iter().filter(|b| b.contains("Teammate entry")).count();
    assert_eq!(
        touching_teammate, 1,
        "the pulled row reached the embedder once, not once per pass: {docs:?}"
    );
    drop(loopback);
}

// ── 7. an empty body embeds the same way it does on push ────────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn an_empty_body_entry_embeds_on_pull_exactly_as_it_does_on_push() {
    let loopback = spawn_content_embedder("proj", None).await;

    // Pull side: a remote entry with an empty body.
    let remote = MockServer::start().await;
    mount_pages(&remote, &[vec![entry(0, "Bodyless", "")]]).await;
    let (pull_tmp, pulled_store) = fresh_store();
    let pull_client = CloudSyncClient::new(&remote.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    let pull_summary =
        pull_and_apply_since(&pulled_store, &pull_client, None, &policy(&cfg, &pull_tmp))
            .await
            .unwrap();

    // Push side: the same entry authored locally, in its own store.
    let team = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 0, "results": []
        })))
        .mount(&team)
        .await;
    let (push_tmp, pushed_store) = fresh_store();
    pushed_store
        .add_note("decision", "Bodyless", "", &[], &[], None, None)
        .unwrap();
    let push_client = CloudSyncClient::new(&team.uri(), "proj", None, None).unwrap();
    let push_summary = push_local(
        &pushed_store,
        &push_client,
        false,
        false,
        &policy(&cfg, &push_tmp),
    )
    .await
    .unwrap();

    assert_eq!(
        (
            pull_summary.embedded_locally,
            pull_summary.without_local_vector
        ),
        (
            push_summary.embedded_locally,
            push_summary.without_local_vector
        ),
        "an empty body must be no more of an obstacle on pull than on push"
    );
    assert_eq!(
        vector_of(&pulled_store, "Bodyless"),
        vector_of(&pushed_store, "Bodyless"),
        "the two paths must build the same embed document and store the same vector"
    );
    drop(loopback);
}

// ── 8. one row's embed failure neither aborts nor unwinds ───────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn one_rows_embed_failure_leaves_the_applied_page_intact() {
    let loopback = spawn_content_embedder("proj", Some("Poisoned")).await;
    let server = MockServer::start().await;
    mount_pages(
        &server,
        &[vec![
            entry(0, "Healthy one", "leader election timeouts"),
            entry(1, "Poisoned", "this document's embed call is refused"),
            entry(2, "Healthy two", "checkpoint interval tuning"),
        ]],
    )
    .await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    let summary = pull_and_apply_since(&store, &client, None, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!(summary.applied, 3, "the page must still apply atomically");
    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (2, 1)
    );
    let titles: Vec<String> = store
        .rows_for_sync(true)
        .unwrap()
        .into_iter()
        .map(|r| r.title)
        .collect();
    for title in ["Healthy one", "Poisoned", "Healthy two"] {
        assert!(
            titles.contains(&title.to_string()),
            "{title} must still be in the store: {titles:?}"
        );
    }
    assert!(vector_of(&store, "Poisoned").is_none());
    assert!(vector_of(&store, "Healthy one").is_some());
    assert!(vector_of(&store, "Healthy two").is_some());
    drop(loopback);
}

// ── 9. the one-way `plumbing pull` entry point, directly ────────────────────

#[tokio::test]
#[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
async fn the_one_way_pull_entry_point_embeds_what_it_applies() {
    let loopback = spawn_content_embedder("proj", None).await;
    let server = MockServer::start().await;
    mount_pages(
        &server,
        &[vec![entry(
            0,
            "One way",
            "cursor derived from the store itself",
        )]],
    )
    .await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
    let cfg = discovering_cfg();
    // `pull_and_apply`, not `pull_and_apply_since`: this is the function
    // `inkentry plumbing pull` calls, deriving its own cursor.
    let summary = pull_and_apply(&store, &client, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!(summary.applied, 1);
    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (1, 0)
    );
    assert_eq!(
        nearest_title(&store, "One way cursor derived from the store itself").as_deref(),
        Some("One way")
    );
    drop(loopback);
}

// ── 10. no embedder at all: succeed, and say how many are pending ───────────

#[tokio::test]
async fn with_no_embedder_the_pull_succeeds_text_only_and_counts_what_is_pending() {
    let server = MockServer::start().await;
    mount_pages(
        &server,
        &[vec![
            entry(0, "Pending one", "write ahead log fsync policy"),
            entry(1, "Pending two", "connection pool sizing"),
        ]],
    )
    .await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
    let cfg = no_embedder_cfg();
    let summary = pull_and_apply_since(&store, &client, None, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!(summary.applied, 2, "the pull must not fail");
    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (0, 2)
    );
    // The content is there; only the vector is missing.
    assert_eq!(
        store
            .rows_for_sync(true)
            .unwrap()
            .into_iter()
            .find(|r| r.title == "Pending one")
            .map(|r| r.body),
        Some("write ahead log fsync policy".to_string())
    );
    assert!(vector_of(&store, "Pending one").is_none());

    // Both user-facing surfaces must name the count rather than say nothing.
    let clause = pull_embed_summary(&summary);
    assert!(
        clause.contains("2 synced entries pending embedding"),
        "the sync summary must report the pending count: {clause:?}"
    );
    let warning = pending_embedding_warning(summary.without_local_vector);
    assert!(warning.contains("2 synced entries"), "{warning}");
    assert!(
        warning.contains("retries automatically"),
        "the warning must say the next run picks them up: {warning}"
    );
}

// ── the cloud_first carve-out, matched to the other call sites ──────────────

// `cloud_first` with a team `server_url` relocates the store of record off
// `memory.db`, so there is nothing local to repair. `memory reindex` refuses
// under exactly this condition and `open_memory_backend` routes on it; the pull
// must not disagree with either.
#[tokio::test]
async fn cloud_first_with_a_server_url_leaves_pulled_rows_alone() {
    let server = MockServer::start().await;
    mount_pages(
        &server,
        &[vec![entry(0, "Elsewhere", "not this store's business")]],
    )
    .await;

    let (tmp, store) = fresh_store();
    let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
    let cfg = Config {
        project_id: Some("proj".to_string()),
        server_url: Some("http://inkentry.internal:4655".to_string()),
        mode: Some(SyncMode::CloudFirst),
        ..Default::default()
    };
    let summary: PullSummary = pull_and_apply_since(&store, &client, None, &policy(&cfg, &tmp))
        .await
        .unwrap();

    assert_eq!(summary.applied, 1);
    assert_eq!(
        (summary.embedded_locally, summary.without_local_vector),
        (0, 0),
        "a skipped pass must report neither embedded nor pending"
    );
    assert!(vector_of(&store, "Elsewhere").is_none());
}
