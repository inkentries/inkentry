use anyhow::Result;

use super::local_embed::{LocalEmbedPolicy, RepairCounts, repair_local_embeddings};
use crate::storage::{CloudSyncClient, MemoryStore, SyncRow};

#[derive(Debug, Default)]
pub(in crate::cli::cmd) struct PullSummary {
    pub applied: usize,
    pub embedded_locally: usize,
    pub without_local_vector: usize,
}

impl PullSummary {
    // `applied` and `embedded_locally` are work, so they add. `without_local_vector`
    // is standing state the second pass re-scans; summing would double-count.
    pub(super) fn merge(self, other: Self) -> Self {
        Self {
            applied: self.applied + other.applied,
            embedded_locally: self.embedded_locally + other.embedded_locally,
            without_local_vector: other.without_local_vector,
        }
    }
}

pub(in crate::cli::cmd) async fn pull_and_apply(
    local: &MemoryStore,
    client: &CloudSyncClient,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<PullSummary> {
    let cursor = local.max_remote_id()?;
    pull_and_apply_since(local, client, cursor.as_deref(), local_embed).await
}

pub(super) async fn pull_and_apply_since(
    local: &MemoryStore,
    client: &CloudSyncClient,
    cursor: Option<&str>,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<PullSummary> {
    let (mut applied, server_total) = drain_pages_since(local, client, cursor).await?;

    // A forward scan cannot reach rows whose ids sort behind the cursor (what a
    // `plumbing push --force` restore produces), so when the server holds more
    // active notes, re-pull from the start. The re-pull's own total is ignored so
    // a persistent skew cannot loop.
    if let Some(total) = server_total
        && total > local.count()?
    {
        let (re_applied, _) = drain_pages_since(local, client, None).await?;
        applied += re_applied;
    }

    // Once, after the last page: one embedder probe, and a failing embed cannot
    // unwind pages already applied.
    let repair = embed_synced_rows(local, local_embed).await?;
    Ok(PullSummary {
        applied,
        embedded_locally: repair.embedded,
        without_local_vector: repair.without_vector,
    })
}

async fn drain_pages_since(
    local: &MemoryStore,
    client: &CloudSyncClient,
    cursor: Option<&str>,
) -> Result<(usize, Option<i64>)> {
    let mut cursor = cursor.map(str::to_string);
    let mut applied = 0usize;
    let mut total = None;
    loop {
        let page = client.pull_since(cursor.as_deref()).await?;
        // Project-wide snapshot repeated on each page; an older server sends none.
        if page.total.is_some() {
            total = page.total;
        }
        let page_len = page.entries.len();

        for e in &page.entries {
            let created_secs = parse_iso_to_secs(&e.created_at);
            let inserted = local.apply_remote_note(
                &e.id,
                &e.kind,
                &e.title,
                e.body.as_deref().unwrap_or(""),
                e.source_commit.as_deref(),
                created_secs,
                e.is_archived(),
            )?;
            if inserted {
                applied += 1;
            }
        }

        // Terminate on the real entry count, never the wire `count`.
        if (page_len as i64) < CloudSyncClient::MEMORY_SINCE_PULL_LIMIT {
            break;
        }
        // A full page never proves it is the last (the server caps at the limit),
        // so follow up; `entries` is non-empty here, so `last()` is `Some`.
        cursor = page.entries.last().map(|e| e.id.clone());
    }

    Ok((applied, total))
}

// Scope is `remote_id IS NOT NULL`, the complement of push's `remote_id IS NULL`;
// an archived never-synced row is in neither. Keyed on "no usable vector", not
// "returned by this pull", so it also catches up rows an earlier pull left
// text-only, and the second pull of a sync sees rows the push just stamped.
async fn embed_synced_rows(
    local: &MemoryStore,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<RepairCounts> {
    let rows = local.rows_for_sync(false)?;
    let synced: Vec<&SyncRow> = rows.iter().filter(|r| r.remote_id.is_some()).collect();
    repair_local_embeddings(local, &synced, local_embed).await
}

// Falls back to "now" so one odd row never aborts the whole sync.
pub(in crate::cli::cmd::memory) fn parse_iso_to_secs(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|_| crate::storage::now_secs())
}

#[cfg(test)]
mod tests {
    use super::super::local_embed::LocalEmbedPolicy;
    use super::super::push::push_local;
    use super::super::round::sync_round;
    use super::super::test_support::{fresh_store, register_sqlite_vec, spawn_inkentry_server};
    use super::*;

    #[test]
    fn parse_iso_to_secs_handles_utc_z() {
        // 2021-01-01T00:00:00Z = 1609459200
        assert_eq!(parse_iso_to_secs("2021-01-01T00:00:00Z"), 1_609_459_200);
    }

    #[test]
    fn parse_iso_to_secs_handles_offset() {
        // 2021-01-01T01:00:00+01:00 == 2021-01-01T00:00:00Z
        assert_eq!(
            parse_iso_to_secs("2021-01-01T01:00:00+01:00"),
            1_609_459_200
        );
    }

    #[test]
    fn parse_iso_to_secs_falls_back_on_garbage() {
        assert!(parse_iso_to_secs("not-a-timestamp") > 0);
    }

    // Real inkentry-server router: a wiremock cannot reproduce a batch ack whose id
    // disagrees with the `sync_id` the `/memory/since` cursor keys on.
    #[tokio::test]
    async fn established_client_pulls_teammates_entries_added_after_its_first_sync() {
        register_sqlite_vec();
        let addr = spawn_inkentry_server().await;
        let base_url = format!("http://{addr}");

        let tmp_a = tempfile::TempDir::new().unwrap();
        let store_a = MemoryStore::open(&tmp_a.path().join("memory.db")).unwrap();
        store_a
            .add_note(
                "decision",
                "A1",
                "client A's own entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj", None, None).unwrap();

        let push1 = push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(
            push1.created, 1,
            "client A's own entry must land on the server"
        );
        let pull1 = pull_and_apply(&store_a, &client_a, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(pull1, 0, "nothing new on the server yet for the first pull");

        let tmp_b = tempfile::TempDir::new().unwrap();
        let store_b = MemoryStore::open(&tmp_b.path().join("memory.db")).unwrap();
        store_b
            .add_note("decision", "B1", "teammate B's entry", &[], &[], None, None)
            .unwrap();
        let client_b = CloudSyncClient::new(&base_url, "proj", None, None).unwrap();
        let push_b = push_local(&store_b, &client_b, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(
            push_b.created, 1,
            "teammate B's entry must land on the server"
        );

        let pull2 = pull_and_apply(&store_a, &client_a, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(
            pull2, 1,
            "an established client must still pull entries a teammate pushed afterward"
        );
        let titles: Vec<String> = store_a
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(
            titles.contains(&"B1".to_string()),
            "client A must now have teammate B's entry locally: {titles:?}"
        );
    }

    #[tokio::test]
    async fn two_established_clients_each_pull_correctly_across_multiple_rounds() {
        register_sqlite_vec();
        let addr = spawn_inkentry_server().await;
        let base_url = format!("http://{addr}");

        let tmp_a = tempfile::TempDir::new().unwrap();
        let store_a = MemoryStore::open(&tmp_a.path().join("memory.db")).unwrap();
        store_a
            .add_note("decision", "A1", "client A's entry", &[], &[], None, None)
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj3", None, None).unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );
        assert_eq!(
            pull_and_apply(&store_a, &client_a, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .applied,
            0
        );

        // C joins pull-only: pushing new content in the same round as older unpulled
        // remote content makes its fresh sync_id the cursor and shadows that content.
        let tmp_c = tempfile::TempDir::new().unwrap();
        let store_c = MemoryStore::open(&tmp_c.path().join("memory.db")).unwrap();
        let client_c = CloudSyncClient::new(&base_url, "proj3", None, None).unwrap();
        let pull_c1 = pull_and_apply(&store_c, &client_c, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(pull_c1, 1, "client C must pull client A's A1 on establish");

        store_c
            .add_note("decision", "C1", "client C's entry", &[], &[], None, None)
            .unwrap();
        assert_eq!(
            push_local(&store_c, &client_c, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );
        assert_eq!(
            pull_and_apply(&store_c, &client_c, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .applied,
            0,
            "nothing further for C to pull immediately after its own push"
        );

        let tmp_b = tempfile::TempDir::new().unwrap();
        let store_b = MemoryStore::open(&tmp_b.path().join("memory.db")).unwrap();
        store_b
            .add_note(
                "decision",
                "B1",
                "teammate B's first entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_b = CloudSyncClient::new(&base_url, "proj3", None, None).unwrap();
        assert_eq!(
            push_local(&store_b, &client_b, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        let pull_a_round2 = pull_and_apply(&store_a, &client_a, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(
            pull_a_round2, 2,
            "client A must pull both C1 and B1 on its second sync"
        );
        let pull_c_round2 = pull_and_apply(&store_c, &client_c, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(
            pull_c_round2, 1,
            "client C must pull only B1 (it already has A1 and its own C1)"
        );

        store_b
            .add_note(
                "decision",
                "B2",
                "teammate B's second entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            push_local(&store_b, &client_b, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        let pull_a_round3 = pull_and_apply(&store_a, &client_a, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(
            pull_a_round3, 1,
            "client A's cursor must advance correctly again on a third round"
        );
        let pull_c_round3 = pull_and_apply(&store_c, &client_c, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(
            pull_c_round3, 1,
            "client C's cursor must advance correctly again on a third round"
        );

        let titles_a: Vec<String> = store_a
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(
            ["A1", "C1", "B1", "B2"]
                .iter()
                .all(|t| titles_a.contains(&t.to_string())),
            "client A must end up with all four entries exactly once each: {titles_a:?}"
        );
        let titles_c: Vec<String> = store_c
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(
            ["A1", "C1", "B1", "B2"]
                .iter()
                .all(|t| titles_c.contains(&t.to_string())),
            "client C must end up with all four entries exactly once each: {titles_c:?}"
        );
    }

    // Lexically increasing ids, so `since_id` cursors compare like real UUIDv7s.
    fn page_ids(start: usize, count: usize) -> Vec<String> {
        (start..start + count)
            .map(|i| format!("01890000-0000-7000-8000-{i:012x}"))
            .collect()
    }

    fn entries_json(ids: &[String]) -> serde_json::Value {
        let entries: Vec<_> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "kind": "note",
                    "title": format!("T-{id}"),
                    "body": "b",
                    "created_at": "2026-06-19T01:00:00Z",
                })
            })
            .collect();
        serde_json::json!({ "entries": entries, "count": entries.len() })
    }

    const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

    // One mock per page, matched on the `since_id` it must be requested with;
    // each is hit exactly `times`.
    async fn mount_pages_times(server: &wiremock::MockServer, pages: &[Vec<String>], times: u64) {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};

        let mut cursor = NIL_UUID.to_string();
        for ids in pages {
            Mock::given(method("GET"))
                .and(path("/v1/projects/proj/memory/since"))
                .and(query_param("since_id", cursor.clone()))
                .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(ids)))
                .expect(times)
                .mount(server)
                .await;
            if let Some(last) = ids.last() {
                cursor = last.clone();
            }
        }
    }

    async fn mount_pages(server: &wiremock::MockServer, pages: &[Vec<String>]) {
        mount_pages_times(server, pages, 1).await;
    }

    #[tokio::test]
    async fn pull_and_apply_since_single_page_matches_prior_behavior() {
        let server = wiremock::MockServer::start().await;
        let page = page_ids(0, 40);
        mount_pages(&server, &[page]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 40);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pull_and_apply_since_two_pages_advances_cursor_to_last_id_of_prior_page() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 40);
        mount_pages(&server, &[page1.clone(), page2.clone()]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 140);
        assert_eq!(store.count().unwrap(), 140, "no duplicates applied");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn pull_and_apply_since_three_pages_loops_past_two_iterations() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 100);
        let page3 = page_ids(200, 45);
        mount_pages(&server, &[page1, page2, page3]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 245);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn pull_and_apply_since_full_page_triggers_exactly_one_more_request() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2: Vec<String> = vec![];
        mount_pages(&server, &[page1, page2]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 100);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn pull_and_apply_since_empty_first_page_terminates_after_one_request() {
        let server = wiremock::MockServer::start().await;
        mount_pages(&server, &[vec![]]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    // `memory_sync` cannot be driven in this binary, so assert on the value its
    // message interpolates.
    #[tokio::test]
    async fn sync_round_pulled_count_reflects_every_page_not_just_the_first() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 60);
        {
            use wiremock::matchers::{method, path, query_param};
            use wiremock::{Mock, ResponseTemplate};
            // No `expect`: the confirmation pull re-derives the same nil cursor and
            // legitimately repeats these requests.
            Mock::given(method("GET"))
                .and(path("/v1/projects/proj/memory/since"))
                .and(query_param("since_id", NIL_UUID))
                .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(&page1)))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/projects/proj/memory/since"))
                .and(query_param("since_id", page1.last().unwrap().clone()))
                .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(&page2)))
                .mount(&server)
                .await;
        }

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let outcome = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();

        assert_eq!(
            outcome.pulled.applied, 160,
            "both pull passes re-fetch the same 160-entry backlog off the \
             unchanged pre-round cursor; apply_remote_note's dedupe means the \
             SECOND pass applies 0 new rows, so pulled must be exactly the \
             true total, not double-counted nor short of the second page"
        );
        assert_eq!(store.count().unwrap(), 160);
    }

    #[tokio::test]
    async fn pull_and_apply_one_way_also_paginates_fully() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 45);
        mount_pages(&server, &[page1, page2]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply(&store, &client, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 145);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sync_round_first_sync_post_push_pull_paginates_fully_on_its_own() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 0, "failed": 0, "results": []
            })))
            .mount(&server)
            .await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 30);
        mount_pages(&server, &[page1, page2]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let outcome = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();

        assert_eq!(outcome.pushed.attempted, 0);
        assert_eq!(
            outcome.pulled.applied, 130,
            "the post-push pull on a first sync must exhaust its own pagination too"
        );
        assert_eq!(store.count().unwrap(), 130);
    }

    #[tokio::test]
    async fn pull_and_apply_since_rerun_after_partial_prior_run_does_not_double_count() {
        let server = wiremock::MockServer::start().await;
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 40);
        // Each page is legitimately re-requested once per run below.
        mount_pages_times(&server, &[page1.clone(), page2.clone()], 2).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let first_run = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(first_run, 140);

        let rerun = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(rerun, 0, "already-applied entries must not be re-counted");
        assert_eq!(store.count().unwrap(), 140, "and never re-inserted");
    }

    #[tokio::test]
    async fn pull_and_apply_since_404_on_first_page_is_still_zero_not_an_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 0);
    }

    #[tokio::test]
    async fn pull_and_apply_since_none_cursor_still_starts_at_nil_uuid() {
        let server = wiremock::MockServer::start().await;
        mount_pages(&server, &[page_ids(0, 5)]).await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 5);
    }

    // The push already landed and stamped its row before the pull failed: the
    // error must surface and say so, leaving local state as `push_local` left it.
    #[tokio::test]
    async fn sync_round_first_sync_post_push_pull_failure_surfaces_the_error_without_losing_the_push()
     {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "T1", "own new entry", &[], &[], None, None)
            .unwrap();
        let ext = store.rows_for_sync(false).unwrap()[0].id.to_string();
        let cloud_id = "01890000-0000-7000-8000-0000000000b1";

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": ext, "id": cloud_id}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let err = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .expect_err("a real post-push pull error must not be swallowed as success");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("push already reached the server") && msg.contains("1 created"),
            "error must say the push already succeeded, not read as a total \
             failure: {msg}"
        );

        assert!(
            store.note_id_for_remote_id(cloud_id).unwrap().is_some(),
            "the already-succeeded push must not be undone or left unstamped \
             just because the confirmation pull afterward failed"
        );
        assert_eq!(
            store.count().unwrap(),
            1,
            "no duplicate/corrupted local row"
        );
    }

    // Pages apply as they arrive, so earlier pages survive a later-page failure
    // (buffering all pages first would lose them).
    #[tokio::test]
    async fn pull_and_apply_since_error_on_a_later_page_keeps_earlier_pages_applied_and_is_retryable()
     {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Page 1 must be full, or the loop never requests a second page.
        let page1 = page_ids(0, 100);
        let page2 = page_ids(100, 5);

        let server1 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", NIL_UUID))
            .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(&page1)))
            .expect(1)
            .mount(&server1)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", page1.last().unwrap().clone()))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server1)
            .await;

        let (_tmp, store) = fresh_store();
        let client1 = CloudSyncClient::new(&server1.uri(), "proj", None, None).unwrap();
        pull_and_apply_since(&store, &client1, None, &LocalEmbedPolicy::Skip)
            .await
            .expect_err("a later-page failure must surface as Err, not a silent partial success");

        assert_eq!(
            store.count().unwrap(),
            100,
            "page 1's entries must already be durably applied even though the \
             overall call failed on page 2"
        );
        assert_eq!(
            store.max_remote_id().unwrap().as_deref(),
            Some(page1.last().unwrap().as_str()),
            "the store-derived cursor reflects exactly the pages that landed, \
             so a retry resumes from the right place"
        );

        let server2 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", page1.last().unwrap().clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(entries_json(&page2)))
            .expect(1)
            .mount(&server2)
            .await;
        let client2 = CloudSyncClient::new(&server2.uri(), "proj", None, None).unwrap();

        let cursor = store.max_remote_id().unwrap();
        let applied =
            pull_and_apply_since(&store, &client2, cursor.as_deref(), &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .applied;
        assert_eq!(applied, 5, "the retry applies exactly the remainder");
        assert_eq!(
            store.count().unwrap(),
            105,
            "no duplicates from re-fetching across the two runs"
        );
    }

    // The whole body deserializes before any entry applies; guards against a
    // streaming parse applying a prefix before hitting the bad entry.
    #[tokio::test]
    async fn pull_and_apply_since_malformed_entry_missing_id_fails_the_page_atomically() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let malformed = serde_json::json!({
            "entries": [
                {
                    "kind": "note",
                    "title": "no id field at all",
                    "body": "b",
                    "created_at": "2026-06-19T01:00:00Z"
                }
            ],
            "count": 1
        });
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(malformed))
            .expect(1)
            .mount(&server)
            .await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .expect_err("a page that fails to parse must surface as Err");

        assert_eq!(
            store.count().unwrap(),
            0,
            "nothing from an unparseable page may be partially applied"
        );
    }

    // Termination keys off `entries.len()`, not the redundant wire `count`.
    #[tokio::test]
    async fn pull_and_apply_since_terminates_on_actual_entries_len_not_a_lying_count_field() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let ids = page_ids(0, 50);
        let mut body = entries_json(&ids);
        // `count` falsely claims more remain; keying off it would send a second request.
        body["count"] = serde_json::json!(9999);
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let (_tmp, store) = fresh_store();
        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply_since(&store, &client, None, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 50);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a lying count must not trigger a second request past the real \
             short page"
        );
    }

    fn entries_json_with_total(ids: &[String], total: i64) -> serde_json::Value {
        let mut v = entries_json(ids);
        v["total"] = serde_json::json!(total);
        v
    }

    fn empty_json_with_total(total: i64) -> serde_json::Value {
        serde_json::json!({ "entries": [], "count": 0, "total": total })
    }

    #[tokio::test]
    async fn higher_server_total_triggers_a_full_re_pull_of_rows_behind_the_cursor() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};

        let ids = page_ids(1, 9); // ...001 .. ...009, lexically ordered
        let (low1, low2, high) = (ids[0].clone(), ids[1].clone(), ids[8].clone());

        let (_tmp, store) = fresh_store();
        store
            .apply_remote_note(
                &high,
                "note",
                "high",
                "b",
                None,
                crate::storage::now_secs(),
                false,
            )
            .unwrap();
        assert_eq!(store.count().unwrap(), 1);

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", high.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_json_with_total(3)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", NIL_UUID))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(entries_json_with_total(&[low1, low2, high], 3)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply(&store, &client, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 2, "only the two rows behind the cursor are new");
        assert_eq!(store.count().unwrap(), 3, "no duplicate of the held row");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "one forward pull, then exactly one full re-pull"
        );
    }

    #[tokio::test]
    async fn matching_total_does_not_trigger_a_re_pull() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};

        let ids = page_ids(1, 9);
        let high = ids[8].clone();

        let (_tmp, store) = fresh_store();
        store
            .apply_remote_note(
                &high,
                "note",
                "high",
                "b",
                None,
                crate::storage::now_secs(),
                false,
            )
            .unwrap();

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", high))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_json_with_total(1)))
            .expect(1)
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply(&store, &client, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 0);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a matching total must not trigger a second, full re-pull"
        );
    }

    #[tokio::test]
    async fn absent_total_never_triggers_a_re_pull() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};

        let ids = page_ids(1, 9);
        let high = ids[8].clone();

        let (_tmp, store) = fresh_store();
        store
            .apply_remote_note(
                &high,
                "note",
                "high",
                "b",
                None,
                crate::storage::now_secs(),
                false,
            )
            .unwrap();

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(query_param("since_id", high))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let applied = pull_and_apply(&store, &client, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;

        assert_eq!(applied, 0);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "an absent total is not a divergence signal and must not re-pull"
        );
    }
}
