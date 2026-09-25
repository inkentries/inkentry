use anyhow::{Context, Result};

use crate::storage::{CloudSyncClient, MemoryStore};

use super::local_embed::LocalEmbedPolicy;
use super::pull::{PullSummary, pull_and_apply_since};
use super::push::{PushSummary, push_local};

#[derive(Debug)]
pub(super) struct SyncRoundOutcome {
    pub(super) pushed: PushSummary,
    pub(super) pulled: PullSummary,
}

pub(super) async fn sync_round(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<SyncRoundOutcome> {
    // The cursor is `MAX(remote_id)` over local rows, so once this round's push
    // stamps newer ids a re-derived cursor would permanently shadow any teammate
    // entry that landed between our pull and push. Both pulls reuse this one.
    let pre_round_cursor = local.max_remote_id()?;

    if pre_round_cursor.is_none() {
        return sync_round_first(
            local,
            client,
            include_archived,
            accepts_pushed_vectors,
            local_embed,
        )
        .await;
    }

    let pulled_first =
        pull_and_apply_since(local, client, pre_round_cursor.as_deref(), local_embed).await?;

    let pushed = push_local(
        local,
        client,
        include_archived,
        accepts_pushed_vectors,
        local_embed,
    )
    .await?;

    // The push may already have landed, so the error context keeps a failure
    // here from reading as "nothing happened".
    let pulled_second =
        pull_and_apply_since(local, client, pre_round_cursor.as_deref(), local_embed)
            .await
            .with_context(|| {
                format!(
                    "confirmation pull failed after this round's push already reached \
                 the server ({} attempted: {} created, {} skipped, {} failed) - \
                 the push is not affected by this error; re-running sync will retry \
                 the pull without re-pushing already-landed entries",
                    pushed.attempted, pushed.created, pushed.skipped, pushed.failed
                )
            })?;

    Ok(SyncRoundOutcome {
        pushed,
        pulled: pulled_first.merge(pulled_second),
    })
}

// A new project only exists server-side once this round's push provisions it,
// so pulling first would hit a project the server cannot resolve.
async fn sync_round_first(
    local: &MemoryStore,
    client: &CloudSyncClient,
    include_archived: bool,
    accepts_pushed_vectors: bool,
    local_embed: &LocalEmbedPolicy<'_>,
) -> Result<SyncRoundOutcome> {
    let pushed = push_local(
        local,
        client,
        include_archived,
        accepts_pushed_vectors,
        local_embed,
    )
    .await?;

    let pulled = pull_and_apply_since(local, client, None, local_embed)
        .await
        .with_context(|| {
            format!(
                "post-push pull failed on this project's first sync, after the push already \
                 reached the server ({} attempted: {} created, {} skipped, {} failed) - \
                 the push is not affected by this error; re-running sync will retry \
                 the pull without re-pushing already-landed entries",
                pushed.attempted, pushed.created, pushed.skipped, pushed.failed
            )
        })?;

    Ok(SyncRoundOutcome { pushed, pulled })
}

#[cfg(test)]
mod tests {
    use super::super::pull::pull_and_apply;
    use super::super::test_support::{fresh_store, spawn_inkentry_server};
    use super::*;

    #[tokio::test]
    async fn sync_round_pulls_teammates_prior_entry_on_a_first_round_with_local_content() {
        let addr = spawn_inkentry_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp_a, store_a) = fresh_store();
        store_a
            .add_note(
                "decision",
                "A1",
                "teammate's prior entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj-primary", None, None).unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        let (_tmp_c, store_c) = fresh_store();
        store_c
            .add_note(
                "decision",
                "C1",
                "client C's own new entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_c = CloudSyncClient::new(&base_url, "proj-primary", None, None).unwrap();

        let outcome = sync_round(&store_c, &client_c, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(outcome.pushed.created, 1, "C's own entry must land");
        assert_eq!(
            outcome.pulled.applied, 1,
            "C must pull A's prior entry within this same round, not 0"
        );
        let titles: Vec<String> = store_c
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(titles.contains(&"A1".to_string()) && titles.contains(&"C1".to_string()));
    }

    #[tokio::test]
    async fn sync_round_twice_with_nothing_new_is_idempotent_and_never_double_counts() {
        let addr = spawn_inkentry_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "A1", "own entry", &[], &[], None, None)
            .unwrap();
        let client = CloudSyncClient::new(&base_url, "proj-idem", None, None).unwrap();

        let r1 = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(r1.pushed.created, 1);
        assert_eq!(
            r1.pulled.applied, 0,
            "the second pull re-fetches this round's own just-pushed row via \
             the pre-round cursor, but it must not be double-counted"
        );
        assert_eq!(store.count().unwrap(), 1, "no duplicate local row");

        let r2 = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(
            (
                r2.pushed.attempted,
                r2.pushed.already_synced,
                r2.pulled.applied
            ),
            (0, 1, 0),
            "a second round with nothing new must be a full no-op"
        );
        assert_eq!(store.count().unwrap(), 1);
    }

    // Composes `sync_round`'s three calls by hand so the teammate's push can be
    // interleaved deterministically.
    #[tokio::test]
    async fn sync_round_catches_a_teammate_push_landing_between_its_own_pull_and_push() {
        let addr = spawn_inkentry_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "Client1", "own new entry", &[], &[], None, None)
            .unwrap();
        let client = CloudSyncClient::new(&base_url, "proj-race", None, None).unwrap();

        let pre_round_cursor = store.max_remote_id().unwrap();
        let pulled_first = pull_and_apply_since(
            &store,
            &client,
            pre_round_cursor.as_deref(),
            &LocalEmbedPolicy::Skip,
        )
        .await
        .unwrap()
        .applied;
        assert_eq!(pulled_first, 0);

        let (_tmp_b, store_b) = fresh_store();
        store_b
            .add_note(
                "decision",
                "B1",
                "teammate's race-window entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let client_b = CloudSyncClient::new(&base_url, "proj-race", None, None).unwrap();
        assert_eq!(
            push_local(&store_b, &client_b, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        let pushed = push_local(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();
        assert_eq!(pushed.created, 1);

        // A re-derived max_remote_id() would include our own push and shadow B1.
        let pulled_second = pull_and_apply_since(
            &store,
            &client,
            pre_round_cursor.as_deref(),
            &LocalEmbedPolicy::Skip,
        )
        .await
        .unwrap()
        .applied;
        assert_eq!(
            pulled_second, 1,
            "the race-window teammate push must be caught by the second pull, \
             not permanently lost"
        );

        let titles: Vec<String> = store
            .rows_for_sync(false)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert!(titles.contains(&"B1".to_string()));
    }

    #[tokio::test]
    async fn pull_and_apply_one_way_pull_still_derives_its_own_single_cursor() {
        let addr = spawn_inkentry_server().await;
        let base_url = format!("http://{addr}");

        let (_tmp_a, store_a) = fresh_store();
        store_a
            .add_note("decision", "A1", "first", &[], &[], None, None)
            .unwrap();
        let client_a = CloudSyncClient::new(&base_url, "proj-pull", None, None).unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );
        store_a
            .add_note("decision", "A2", "second", &[], &[], None, None)
            .unwrap();
        assert_eq!(
            push_local(&store_a, &client_a, false, false, &LocalEmbedPolicy::Skip)
                .await
                .unwrap()
                .created,
            1
        );

        let (_tmp_c, store_c) = fresh_store();
        let client_c = CloudSyncClient::new(&base_url, "proj-pull", None, None).unwrap();
        let pulled = pull_and_apply(&store_c, &client_c, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(pulled, 2);

        let pulled_again = pull_and_apply(&store_c, &client_c, &LocalEmbedPolicy::Skip)
            .await
            .unwrap()
            .applied;
        assert_eq!(pulled_again, 0);
    }

    // Fails with 400 until a push has provisioned the project.
    struct SinceUntilProvisioned {
        provisioned: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl wiremock::Respond for SinceUntilProvisioned {
        fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
            if self.provisioned.load(std::sync::atomic::Ordering::SeqCst) {
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 }))
            } else {
                wiremock::ResponseTemplate::new(400)
                    .set_body_string("invalid project id: expected a UUID, got a slug")
            }
        }
    }

    struct BatchProvisions {
        provisioned: std::sync::Arc<std::sync::atomic::AtomicBool>,
        external_id: String,
    }

    impl wiremock::Respond for BatchProvisions {
        fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.provisioned
                .store(true, std::sync::atomic::Ordering::SeqCst);
            wiremock::ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": self.external_id, "id": "cloud-t1"}]
            }))
        }
    }

    // `/memory/since` 400s while unprovisioned, so success proves the pre-push
    // pull was skipped rather than tolerated.
    #[tokio::test]
    async fn sync_round_first_sync_skips_the_pre_push_pull_and_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let (_tmp, store) = fresh_store();
        store
            .add_note(
                "decision",
                "T1",
                "first entry, never synced",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let ext = store.rows_for_sync(false).unwrap()[0].id.to_string();

        let provisioned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(SinceUntilProvisioned {
                provisioned: provisioned.clone(),
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(BatchProvisions {
                provisioned: provisioned.clone(),
                external_id: ext,
            })
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let outcome = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .expect("a first sync must not surface the pre-push pull's 400 to the user");

        assert_eq!(outcome.pushed.created, 1, "the entry must be pushed");
        assert!(
            store.note_id_for_remote_id("cloud-t1").unwrap().is_some(),
            "the project must be provisioned and the entry stamped locally"
        );
    }

    // A first-sync round makes two calls (push, pull); an established one makes three.
    #[tokio::test]
    async fn sync_round_established_client_keeps_pull_before_push_order() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_tmp, store) = fresh_store();
        store
            .apply_remote_note(
                "01890000-0000-7000-8000-000000000001",
                "decision",
                "Seed",
                "seeds a real sync cursor",
                None,
                crate::storage::now_secs(),
                false,
            )
            .unwrap();
        assert!(
            store.max_remote_id().unwrap().is_some(),
            "the store must now be an established client"
        );
        store
            .add_note(
                "decision",
                "A2",
                "new local entry this round",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "a2-ext", "id": "cloud-a2"}]
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .unwrap();

        let reqs = server.received_requests().await.unwrap();
        let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
        assert_eq!(
            methods,
            vec!["GET", "POST", "GET"],
            "an established client must still pull, then push, then pull again: {methods:?}"
        );
    }

    // A crash between the server's 207 and the local `set_remote_id` leaves a
    // store that looks never-synced; the retry's `skipped` result must heal it.
    #[tokio::test]
    async fn sync_round_first_sync_recovers_a_push_that_landed_before_a_crash() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_tmp, store) = fresh_store();
        store
            .add_note(
                "decision",
                "T1",
                "pushed once, crashed before the local remote_id was recorded",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let ext = store.rows_for_sync(false).unwrap()[0].id.to_string();
        assert!(
            store.max_remote_id().unwrap().is_none(),
            "the crash means this row's remote_id was never durably stamped locally"
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "entries": [], "count": 0 })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 1, "failed": 0,
                "results": [{"status": "skipped", "external_id": ext, "id": "cloud-t1-preexisting"}]
            })))
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let outcome = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .expect("a skipped-not-created push must not be treated as a failure");

        assert_eq!(
            outcome.pushed.skipped, 1,
            "the retry sees its own earlier push as already known to the server"
        );
        assert!(
            store
                .note_id_for_remote_id("cloud-t1-preexisting")
                .unwrap()
                .is_some(),
            "a skipped result must still stamp remote_id, recovering the crash-lost state"
        );
        assert!(
            store.max_remote_id().unwrap().is_some(),
            "the store must now be established, so the next sync takes the pull-push-pull path"
        );
    }

    #[tokio::test]
    async fn sync_round_first_sync_surfaces_pull_error_when_push_never_lands() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (_tmp, store) = fresh_store();
        store
            .add_note("decision", "T1", "never lands", &[], &[], None, None)
            .unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("invalid project id: expected a UUID, got a slug"),
            )
            .mount(&server)
            .await;

        let client = CloudSyncClient::new(&server.uri(), "proj", None, None).unwrap();
        let err = sync_round(&store, &client, false, false, &LocalEmbedPolicy::Skip)
            .await
            .expect_err(
                "a pull failure after a totally failed push must still surface as an error",
            );

        assert!(
            format!("{err:#}").contains("post-push pull failed"),
            "the error must carry the first-sync pull-failure context: {err:#}"
        );
        assert!(
            store.max_remote_id().unwrap().is_none(),
            "nothing landed, so a retry must still take the first-sync branch"
        );
    }

    struct BatchProvisionsEcho {
        provisioned: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl wiremock::Respond for BatchProvisionsEcho {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.provisioned
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let entries = body["entries"].as_array().cloned().unwrap_or_default();
            let results: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    let ext = e["external_id"].as_str().unwrap_or_default();
                    serde_json::json!({
                        "status": "created", "external_id": ext, "id": format!("cloud-{ext}")
                    })
                })
                .collect();
            wiremock::ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": results.len(), "skipped": 0, "failed": 0, "results": results
            }))
        }
    }

    #[tokio::test]
    async fn sync_round_two_concurrent_first_syncs_both_succeed_without_pre_push_pull() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let (_tmp_x, store_x) = fresh_store();
        store_x
            .add_note(
                "decision",
                "X1",
                "client X's own new entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let (_tmp_y, store_y) = fresh_store();
        store_y
            .add_note(
                "decision",
                "Y1",
                "client Y's own new entry",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();

        let provisioned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj-race-first/memory/since"))
            .respond_with(SinceUntilProvisioned {
                provisioned: provisioned.clone(),
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj-race-first/memory/batch"))
            .respond_with(BatchProvisionsEcho {
                provisioned: provisioned.clone(),
            })
            .mount(&server)
            .await;

        let client_x = CloudSyncClient::new(&server.uri(), "proj-race-first", None, None).unwrap();
        let client_y = CloudSyncClient::new(&server.uri(), "proj-race-first", None, None).unwrap();

        let (outcome_x, outcome_y) = tokio::join!(
            sync_round(&store_x, &client_x, false, false, &LocalEmbedPolicy::Skip),
            sync_round(&store_y, &client_y, false, false, &LocalEmbedPolicy::Skip),
        );

        assert_eq!(
            outcome_x
                .expect("X's round must not surface the pre-push-pull 400")
                .pushed
                .created,
            1
        );
        assert_eq!(
            outcome_y
                .expect("Y's round must not surface the pre-push-pull 400")
                .pushed
                .created,
            1
        );
    }
}
