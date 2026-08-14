// Tests for the ADR-037 P2 local relay (see `mod.rs`'s module docs for the
// full push/pull/SSE contract these exercise).

use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn target(url: &str, project: &str) -> TeamTarget {
    TeamTarget {
        server_url: url.to_string(),
        project_id: project.to_string(),
        server_ca: None,
    }
}

// A registry that declares exactly the mock servers a test drives, standing in
// for the machine's `.inkentry/config.toml`.
fn registry_for(targets: Vec<TeamTarget>) -> RelayRegistry {
    RelayRegistry::new(RelayPolicy::allowing(targets))
}

// A bare session, for the paths that are exercised without a registry.
fn session_for_target(target: &TeamTarget) -> Arc<RelaySession> {
    Arc::new(RelaySession::new(
        RelayKey::new(&target.server_url, &target.project_id),
        target,
        SESSION_IDLE_TIMEOUT,
    ))
}

fn entry(ext: &str) -> RelayPushEntry {
    RelayPushEntry {
        kind: "decision".into(),
        title: "T".into(),
        body: Some("B".into()),
        external_id: ext.into(),
        source_commit: None,
    }
}

// ── item 18: zero registered projects means zero outbound traffic ──────

#[tokio::test]
async fn empty_registry_makes_no_outbound_calls_and_starts_no_sessions() {
    let registry = registry_for(vec![]);
    assert_eq!(registry.session_count().await, 0);

    // Polling an unregistered project must not create a session either.
    let resp = registry.poll("https://team.example", "proj").await;
    assert!(resp.push_results.is_empty());
    assert!(resp.pulled.is_empty());
    assert_eq!(registry.session_count().await, 0);
}

// ── item 12: push reuses CloudSyncClient/BatchPushItem ─────────────────

#[tokio::test]
async fn push_lands_on_the_team_server_and_is_pollable_and_reoffered_until_acked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
        })))
        .mount(&server)
        .await;
    // No SSE mount: the pull loop's initial catch-up (`/memory/since`)
    // must not block registration or the push itself.
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = registry_for(vec![target(&server.uri(), "proj")]);
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("e1")],
        })
        .await
        .unwrap();

    assert_eq!(registry.session_count().await, 1);

    // The remote push happens in a detached background task; poll until
    // it lands rather than assuming a fixed sleep.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        got = registry.poll(&server.uri(), "proj").await;
        if !got.push_results.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got.push_results.len(), 1);
    assert_eq!(got.push_results[0].external_id, "e1");
    assert_eq!(got.push_results[0].remote_id.as_deref(), Some("cloud-1"));
    assert_eq!(got.push_results[0].status, "created");
    assert!(got.last_synced_at.is_some());

    // A second poll before any ack must return the SAME result again —
    // this is the fix for the destructive-drain data-loss bug: a poll
    // used to clear the buffer in the same call, so a CLI-side apply
    // failure after this first poll would have permanently stranded the
    // row pending forever (nothing left to retry against).
    let second = registry.poll(&server.uri(), "proj").await;
    assert_eq!(
        second.push_results.len(),
        1,
        "an unacked result must still be offered on the next poll"
    );
    assert_eq!(second.push_results[0].external_id, "e1");

    // Only an explicit ack retires it.
    registry
        .ack(&server.uri(), "proj", &["e1".to_string()], &[])
        .await;
    let third = registry.poll(&server.uri(), "proj").await;
    assert!(
        third.push_results.is_empty(),
        "an acked result must not be offered again"
    );
}

#[tokio::test]
async fn push_with_empty_entries_is_a_noop_no_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = registry_for(vec![target(&server.uri(), "proj")]);
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let got = registry.poll(&server.uri(), "proj").await;
    assert!(
        got.push_results.is_empty(),
        "empty entries must never reach the batch endpoint, so nothing is stamped: {got:?}"
    );
}

// ── item 12/16: pull catch-up via /memory/since, cursor round-trips ────

#[tokio::test]
async fn registration_seeds_cursor_and_catch_up_advances_it_and_buffers_pulled_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .and(wiremock::matchers::query_param(
            "since_id",
            "01890000-0000-7000-8000-000000000001",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-000000000002",
                "kind": "note", "title": "Remote",
                "body": "body", "created_at": "2026-06-19T01:00:00Z"
            }],
            "count": 1
        })))
        .mount(&server)
        .await;

    let registry = registry_for(vec![target(&server.uri(), "proj")]);
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: Some("01890000-0000-7000-8000-000000000001".to_string()),
            entries: vec![],
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        got = registry.poll(&server.uri(), "proj").await;
        if !got.pulled.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got.pulled.len(), 1);
    assert_eq!(
        got.pulled[0].remote_id,
        "01890000-0000-7000-8000-000000000002"
    );
    assert!(!got.pulled[0].archived);
}

// ── founder review (PR #728): pull-side data loss without a restart ────
//
// The bug: `GET /local/relay/poll` used to destructively drain buffered
// pulled rows (`std::mem::take`) while the CLI's `apply_remote_note` call
// can fail (SQLITE_BUSY, a killed process) without re-buffering — and the
// session's pull cursor had already advanced past the row when it was
// first buffered, so a restart-free retry would never re-offer it. This
// pins the fix directly at the relay level, independent of any CLI-side
// failure injection: a poll never clears the buffer by itself, so a CLI
// that never acks (modelling "poll succeeded, the local apply after it
// failed") must see the exact same row again, indefinitely, across many
// polls and additional catch-up cycles — never silently dropped. Fails
// against the pre-fix `drain`-on-poll code (the second poll below would
// return empty), passes after.

#[tokio::test]
async fn a_pulled_row_survives_repeated_polls_when_the_cli_never_acks_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-000000000002",
                "kind": "note", "title": "Remote",
                "body": "body", "created_at": "2026-06-19T01:00:00Z"
            }],
            "count": 1
        })))
        .mount(&server)
        .await;

    let registry = registry_for(vec![target(&server.uri(), "proj")]);
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .unwrap();

    // Wait for the initial catch-up to buffer the row.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !registry.poll(&server.uri(), "proj").await.pulled.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the row never arrived to begin with"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Simulate "the CLI polled it, but its local apply failed" by simply
    // never acking, across several more polls (each of which also lets
    // the background pull loop run another catch-up cycle against a
    // cursor that must not have moved past this still-unacked row).
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got = registry.poll(&server.uri(), "proj").await;
        assert_eq!(
            got.pulled.len(),
            1,
            "an unacked pulled row must never disappear from the buffer"
        );
        assert_eq!(
            got.pulled[0].remote_id,
            "01890000-0000-7000-8000-000000000002"
        );
    }

    // Once the CLI confirms it actually applied the row, it is retired.
    registry
        .ack(
            &server.uri(),
            "proj",
            &[],
            &["01890000-0000-7000-8000-000000000002".to_string()],
        )
        .await;
    let after_ack = registry.poll(&server.uri(), "proj").await;
    assert!(
        after_ack.pulled.is_empty(),
        "an acked pulled row must not be offered again"
    );
}

#[tokio::test]
async fn a_later_stale_since_cursor_never_regresses_a_session_that_moved_past_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = registry_for(vec![target(&server.uri(), "proj")]);
    // First registration seeds a cursor ahead of what the second (slower,
    // stale) CLI invocation will offer.
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: Some("01890000-0000-7000-8000-000000000005".to_string()),
            entries: vec![],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let session = registry.lookup(&server.uri(), "proj").await.unwrap();
    assert_eq!(
        session.inner.lock().await.cursor.as_deref(),
        Some("01890000-0000-7000-8000-000000000005")
    );

    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: Some("01890000-0000-7000-8000-000000000001".to_string()),
            entries: vec![],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        session.inner.lock().await.cursor.as_deref(),
        Some("01890000-0000-7000-8000-000000000005"),
        "a stale, earlier cursor must never regress the session's own progress"
    );
}

// ── item 17: one project's relay failure never affects another's ───────

#[tokio::test]
async fn one_sessions_push_failure_does_not_affect_another_sessions_push() {
    let bad_server = MockServer::start().await;
    // No mock mounted at all: every request 404s.
    let good_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
        })))
        .mount(&good_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&good_server)
        .await;

    let registry = registry_for(vec![
        target(&bad_server.uri(), "proj"),
        target(&good_server.uri(), "proj"),
    ]);
    registry
        .push(RelayPushRequest {
            server_url: bad_server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("e1")],
        })
        .await
        .unwrap();
    registry
        .push(RelayPushRequest {
            server_url: good_server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("e2")],
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut good = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        good = registry.poll(&good_server.uri(), "proj").await;
        if !good.push_results.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        good.push_results.len(),
        1,
        "the healthy session's push must land regardless of the other session's failure"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut bad = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        bad = registry.poll(&bad_server.uri(), "proj").await;
        if bad.last_error.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        bad.last_error.is_some(),
        "the failing session records its own error instead of panicking or hanging"
    );
    assert_eq!(registry.session_count().await, 2);
}

// ── item 22: no cross-project SSE/pull leakage ──────────────────────────
// Two projects on the SAME team server: a note pushed to one must never
// appear in the other's pulled buffer. `RelayKey` is `(server_url,
// project_id)`, so distinct project ids always get distinct sessions with
// independent cursors/buffers; this pins that at the observable
// push+pull level rather than trusting the key type alone.

#[tokio::test]
async fn pulled_rows_never_leak_across_projects_on_the_same_team_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/proj-x/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0,
            "results": [{"status": "created", "external_id": "ex", "id": "cloud-x"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj-x/memory/since"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-0000000000x1",
                "kind": "note", "title": "X-only", "body": "b",
                "created_at": "2026-06-19T01:00:00Z"
            }],
            "count": 1
        })))
        .mount(&server)
        .await;
    // proj-y's own /memory/since must never see proj-x's entry (a distinct
    // mock, scoped to a different path, proves the request itself is
    // correctly project-scoped, not just that this mock happens to return
    // nothing).
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj-y/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = registry_for(vec![
        target(&server.uri(), "proj-x"),
        target(&server.uri(), "proj-y"),
    ]);
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj-x".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("ex")],
        })
        .await
        .unwrap();
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj-y".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut x = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        x = registry.poll(&server.uri(), "proj-x").await;
        if !x.pulled.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(x.pulled.len(), 1, "proj-x must see its own entry");
    assert_eq!(x.pulled[0].title, "X-only");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let y = registry.poll(&server.uri(), "proj-y").await;
    assert!(
        y.pulled.is_empty(),
        "proj-y must never see proj-x's pulled entry: {:?}",
        y.pulled
    );
}

// ── founder review (PR #728): SSE frames decode across chunk boundaries ─
//
// `stream_once` used to decode each raw HTTP chunk in isolation
// (`String::from_utf8_lossy(&chunk)` per iteration, before frame
// boundaries were known), which could corrupt a multi-byte UTF-8
// character or a `Last-Event-ID` value split across two chunks. The fix
// accumulates raw bytes and only decodes once a complete `\n\n`-
// terminated frame has been assembled. This pins the byte-safety
// primitive the fix relies on: `find_double_newline` must locate the
// terminator by raw bytes, never by decoding (which would panic or
// silently corrupt data on a not-yet-complete multi-byte sequence sitting
// at the search boundary).

#[test]
fn find_double_newline_locates_the_terminator_around_a_multibyte_char() {
    // "café" — 'é' is the two-byte UTF-8 sequence 0xC3 0xA9. Split the
    // buffer such that this sequence itself sits right before the
    // terminator, the exact shape a chunk-boundary split could produce.
    let mut buf = b"data: caf\xc3\xa9\n\n".to_vec();
    let pos = find_double_newline(&buf).expect("terminator must be found");
    let frame = String::from_utf8_lossy(&buf[..pos + 2]).into_owned();
    assert_eq!(
        frame, "data: café\n\n",
        "the multibyte character must decode intact"
    );

    // No terminator yet (a chunk boundary landed mid-frame, even mid-
    // character): must not find a false match or panic on invalid UTF-8
    // in the not-yet-complete tail.
    buf.truncate(buf.len() - 2); // drop the "\n\n"
    assert_eq!(find_double_newline(&buf), None);
    let mid_char = &buf[..buf.len() - 1]; // split inside 'é''s 2-byte sequence
    assert_eq!(find_double_newline(mid_char), None);
}

// ── oversized/malformed SSE frame errors instead of growing forever ────
//
// A team `server_url` is whatever a project happens to be configured
// with (cloud-api, another inkentry-server, or, if misconfigured, anything
// else); this pins that a peer sending an unterminated line larger than
// `MAX_SSE_BUFFER_BYTES` makes `stream_once` return an error (which
// `run_pull_loop` already turns into `record_error` + backoff + retry,
// never a panic) instead of buffering without bound for as long as the
// connection stays open.

#[tokio::test]
async fn oversized_sse_frame_without_terminator_errors_instead_of_growing_forever() {
    let server = MockServer::start().await;
    let oversized_line = vec![b'x'; MAX_SSE_BUFFER_BYTES + 4096];
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_bytes(oversized_line),
        )
        .mount(&server)
        .await;

    let session = session_for_target(&target(&server.uri(), "proj"));
    let result = stream_once(&session).await;
    assert!(
        result.is_err(),
        "an unterminated frame past the buffer cap must error, not hang or \
         grow without bound"
    );
}

// ── item 13: the reconciler never opens a project's memory.db ──────────
//
// Every public entry point on `RelayRegistry` (`push`, `poll`) takes only
// `server_url` / `project_id` / entry data — never a filesystem path —
// and every type in this module is one of those or wraps `CloudSyncClient`
// (an HTTP client). There is no `MemoryStore`/SQLite-path parameter
// anywhere in this module's public surface for a caller to even supply,
// so a full push+pull round trip (`push_drains_entries_...` and
// `registration_seeds_cursor_and_catch_up_advances_it_...` above)
// completing correctly already proves sync works without this process
// ever being handed — or needing — a `memory.db` path.

// ── the request selects a destination, it never supplies one ────────────
//
// `POST /local/relay/push` used to take `server_url` and `bearer` straight
// from the request body and open connections to them: an unauthenticated
// local caller could make the daemon reach an arbitrary host, from the
// daemon's network position, carrying a bearer of the caller's choosing,
// retried forever. These pin that only a target declared by local
// configuration is ever reachable.

#[tokio::test]
async fn a_server_url_no_local_config_declares_is_refused() {
    let attacker = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/anything/memory/batch"))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": 0, "results": []
        })))
        .mount(&attacker)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/anything/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&attacker)
        .await;

    let registry = registry_for(vec![target("https://team.example", "acme/app")]);
    let err = registry
        .push(RelayPushRequest {
            server_url: attacker.uri(),
            project_id: "anything".to_string(),
            bearer: Some("attacker-chosen-token".to_string()),
            since_cursor: None,
            entries: vec![entry("e1")],
        })
        .await
        .expect_err("an undeclared server_url must be refused");

    assert!(err.to_string().contains("local configuration"), "{err}");
    assert_eq!(registry.session_count().await, 0);
    // Give any (wrongly) spawned background task a chance to make the call
    // this test exists to prove never happens.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        attacker.received_requests().await.unwrap().is_empty(),
        "the daemon must not have connected to the caller-supplied host at all"
    );
}

#[tokio::test]
async fn a_declared_server_with_an_undeclared_project_is_refused() {
    let server = MockServer::start().await;
    let registry = registry_for(vec![target(&server.uri(), "declared")]);

    let err = registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "someone-elses-project".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .expect_err("an undeclared project must be refused");

    assert!(err.to_string().contains("local configuration"), "{err}");
    assert_eq!(registry.session_count().await, 0);
}

#[tokio::test]
async fn a_declared_but_plaintext_non_loopback_target_is_refused() {
    // Even a locally-declared target may not be reached over plaintext http
    // off-host: the entries and the bearer would cross the wire in the clear.
    let registry = registry_for(vec![target("http://team-server:7777", "acme/app")]);

    let err = registry
        .push(RelayPushRequest {
            server_url: "http://team-server:7777".to_string(),
            project_id: "acme/app".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .expect_err("plaintext http to a non-loopback host must be refused");

    assert!(err.to_string().contains("https://"), "{err}");
    assert_eq!(registry.session_count().await, 0);
}

#[tokio::test]
async fn a_disabled_registry_refuses_every_push() {
    let registry = RelayRegistry::disabled();
    assert!(!registry.is_enabled());

    let err = registry
        .push(RelayPushRequest {
            server_url: "https://team.example".to_string(),
            project_id: "acme/app".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .expect_err("a disabled relay must refuse");

    assert!(err.to_string().contains("non-loopback"), "{err}");
    assert_eq!(registry.session_count().await, 0);
}

// The relay is documented local-only, so a daemon that is reachable from
// other machines must not serve it at all — structurally, rather than by
// each handler re-deriving the rule.
#[test]
fn the_relay_is_disabled_on_a_non_loopback_bind() {
    let policy = || RelayPolicy::allowing(vec![target("https://team.example", "acme/app")]);
    for host in ["127.0.0.1", "::1", "localhost", " 127.0.0.5 "] {
        assert!(
            RelayRegistry::for_bind(host, policy()).is_enabled(),
            "{host} is loopback and should serve the relay"
        );
    }
    for host in ["0.0.0.0", "::", "192.168.1.10", "team.example", ""] {
        assert!(
            !RelayRegistry::for_bind(host, policy()).is_enabled(),
            "{host} is reachable off-host and must not serve the relay"
        );
    }
}

// ── the remote error is the operator's, not the caller's ────────────────
//
// `last_error` carried the raw `reqwest` error, which distinguishes
// connection-refused from timed-out from TLS-failed per host and port — a
// blind-SSRF oracle good enough to port-scan with, readable by any local
// process. It now reports one fixed string.

#[tokio::test]
async fn last_error_never_carries_the_remote_error() {
    // A declared target with nothing listening: the underlying failure is a
    // connection error naming the port.
    let dead = MockServer::start().await;
    let dead_uri = dead.uri();
    let port = dead_uri.rsplit(':').next().unwrap().to_string();
    drop(dead);

    let registry = registry_for(vec![target(&dead_uri, "proj")]);
    registry
        .push(RelayPushRequest {
            server_url: dead_uri.clone(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![entry("e1")],
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = RelayPollResponse::default();
    while std::time::Instant::now() < deadline {
        got = registry.poll(&dead_uri, "proj").await;
        if got.last_error.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = got
        .last_error
        .expect("a failing hop must still be reported");
    assert_eq!(err, REMOTE_HOP_FAILED);
    assert!(
        !err.contains(&port) && !err.contains("connect") && !err.contains("tcp"),
        "the remote failure must not be described to the caller: {err}"
    );
}

// ── sessions are bounded and mortal ─────────────────────────────────────

#[tokio::test]
async fn a_sessions_pull_loop_terminates_once_no_cli_is_using_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = RelayRegistry::with_idle_timeout(
        RelayPolicy::allowing(vec![target(&server.uri(), "proj")]),
        Duration::from_millis(150),
    );
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .unwrap();
    assert_eq!(registry.session_count().await, 1);

    // The session is only ever removed by its own pull loop returning, so an
    // empty registry proves the loop terminated rather than reconnecting for
    // the daemon's lifetime.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while registry.session_count().await > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "an untouched session must be retired, not held forever"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_session_in_use_is_not_retired() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/proj/memory/since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"entries": [], "count": 0})),
        )
        .mount(&server)
        .await;

    let registry = RelayRegistry::with_idle_timeout(
        RelayPolicy::allowing(vec![target(&server.uri(), "proj")]),
        Duration::from_millis(200),
    );
    registry
        .push(RelayPushRequest {
            server_url: server.uri(),
            project_id: "proj".to_string(),
            bearer: None,
            since_cursor: None,
            entries: vec![],
        })
        .await
        .unwrap();

    // A CLI polling across more than one idle window keeps its session.
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        registry.poll(&server.uri(), "proj").await;
    }
    assert_eq!(
        registry.session_count().await,
        1,
        "a session a CLI is still polling must survive"
    );
}

#[tokio::test]
async fn the_registry_is_bounded_even_when_local_config_declares_more() {
    // Local config is the first bound on the key space; this is the second.
    // A config declaring more targets than the registry admits must not be
    // able to spawn an unbounded number of sessions and pull loops.
    let declared: Vec<TeamTarget> = (0..MAX_RELAY_SESSIONS + 8)
        .map(|i| target(&format!("https://team-{i}.example"), "acme/app"))
        .collect();
    let registry = registry_for(declared.clone());

    let mut refusals = 0;
    for t in &declared {
        let refused = registry
            .push(RelayPushRequest {
                server_url: t.server_url.clone(),
                project_id: t.project_id.clone(),
                bearer: None,
                since_cursor: None,
                entries: vec![],
            })
            .await
            .is_err();
        if refused {
            refusals += 1;
        }
    }

    assert_eq!(registry.session_count().await, MAX_RELAY_SESSIONS);
    assert_eq!(refusals, 8, "everything past the cap must be refused");
}

// ── the configured custom CA reaches the client ─────────────────────────
//
// The relay hardcoded `None` for the CA, so against an internal-CA team
// server every background hop failed and only manual `inkentry sync` (which
// does pass it) worked. A CA path that cannot be read makes both client
// builds fail, which is what proves the path is threaded rather than
// dropped: with the old `None` these would both succeed.

#[tokio::test]
async fn the_pull_client_is_built_with_the_configured_ca() {
    let session = session_for_target(&TeamTarget {
        server_url: "https://team.example".into(),
        project_id: "acme/app".into(),
        server_ca: Some(std::path::PathBuf::from("/nonexistent/internal-ca.pem")),
    });

    let err = match session.client().await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("an unreadable CA bundle must fail the build, proving it is used"),
    };
    assert!(err.contains("INKENTRY_SERVER_CA"), "{err}");
}

#[tokio::test]
async fn the_stream_client_is_built_with_the_configured_ca() {
    let session = session_for_target(&TeamTarget {
        server_url: "https://team.example".into(),
        project_id: "acme/app".into(),
        server_ca: Some(std::path::PathBuf::from("/nonexistent/internal-ca.pem")),
    });

    let err = stream_once(&session)
        .await
        .expect_err("the SSE leg must apply the CA too");
    assert!(err.to_string().contains("INKENTRY_SERVER_CA"), "{err}");
}

#[tokio::test]
async fn a_valid_ca_bundle_builds_both_clients() {
    let dir = tempfile::TempDir::new().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, TEST_CA_PEM).unwrap();
    let session = session_for_target(&TeamTarget {
        server_url: "https://team.example".into(),
        project_id: "acme/app".into(),
        server_ca: Some(ca),
    });

    assert!(
        session.client().await.is_ok(),
        "a valid CA bundle must build"
    );
    // The stream leg gets as far as the (unreachable) connection, i.e. past
    // the client build the CA participates in.
    let err = stream_once(&session).await.unwrap_err().to_string();
    assert!(!err.contains("INKENTRY_SERVER_CA"), "{err}");
}

// ── retirement's recheck and a live call must agree on one observation ──
//
// `session_for`/`lookup` used to release the sessions-map lock, then call
// `touch()` as a separate step. `retire_if_idle` also locks that map before
// rechecking `idle_for()`, so if it won that gap it would remove the session
// on a stale `last_seen` while the caller that just found/created it still
// held the (now orphaned) `Arc`. These force the gap open by staling
// `last_seen` past the idle timeout right before the call under test, then
// firing the retirement recheck immediately after — proving the recheck
// never wins against a call that just observed the session live.

async fn stale(session: &Arc<RelaySession>, past: Duration) {
    session.inner.lock().await.last_seen = Instant::now() - past;
}

#[tokio::test]
async fn retirement_recheck_cannot_remove_a_session_a_push_just_touched() {
    let t = target("https://team.example", "proj");
    let registry = RelayRegistry::with_idle_timeout(
        RelayPolicy::allowing(vec![t.clone()]),
        Duration::from_millis(50),
    );
    let session = registry.session_for(&t).await.unwrap();
    stale(&session, Duration::from_millis(200)).await;

    // What `push` does before spawning its background work.
    let touched = registry.session_for(&t).await.unwrap();

    assert!(
        !registry.retire_if_idle(&touched).await,
        "a session a push call just found/touched must not be retired"
    );
    assert_eq!(registry.session_count().await, 1);
}

#[tokio::test]
async fn retirement_recheck_cannot_remove_a_session_a_poll_just_touched() {
    let t = target("https://team.example", "proj");
    let registry = RelayRegistry::with_idle_timeout(
        RelayPolicy::allowing(vec![t.clone()]),
        Duration::from_millis(50),
    );
    let session = registry.session_for(&t).await.unwrap();
    stale(&session, Duration::from_millis(200)).await;

    // What `poll` does before calling `touch()`.
    let found = registry.lookup(&t.server_url, &t.project_id).await.unwrap();

    assert!(
        !registry.retire_if_idle(&found).await,
        "a session a poll call just found/touched must not be retired"
    );
    assert_eq!(registry.session_count().await, 1);
}

#[tokio::test]
async fn retirement_recheck_cannot_remove_a_session_an_ack_just_touched() {
    let t = target("https://team.example", "proj");
    let registry = RelayRegistry::with_idle_timeout(
        RelayPolicy::allowing(vec![t.clone()]),
        Duration::from_millis(50),
    );
    let session = registry.session_for(&t).await.unwrap();
    stale(&session, Duration::from_millis(200)).await;

    // What `ack` does before calling `touch()`.
    let found = registry.lookup(&t.server_url, &t.project_id).await.unwrap();

    assert!(
        !registry.retire_if_idle(&found).await,
        "a session an ack call just found/touched must not be retired"
    );
    assert_eq!(registry.session_count().await, 1);
}

// A throwaway self-signed CA: proves the bundle is parsed and accepted as a
// trust anchor. Not trusted by anything real.
const TEST_CA_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\n\
MIIDFTCCAf2gAwIBAgIUdz5ZLoL+3T+MwWN0dJjElxlwsRwwDQYJKoZIhvcNAQEL\n\
BQAwGjEYMBYGA1UEAwwPc3BlbHVuay10ZXN0LWNhMB4XDTI2MDcxMzE3MjkyMFoX\n\
DTM2MDcxMDE3MjkyMFowGjEYMBYGA1UEAwwPc3BlbHVuay10ZXN0LWNhMIIBIjAN\n\
BgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAz/BAzTJJbgWUWnUqV0qFJHT+TIDT\n\
WQJbIRVBb9MezLblAGun2RG22U47jubOKoSa4DrenrEJIafd74IR9aLUdcRp6lyN\n\
WsuzY6P26ntZ1epHUjYeBgqpu71v3FK2pBvQ9PP//AhQN7apE6V4UocKd7OxbSk7\n\
g1bZSYSXoFQtSZzV9KCWNpuqUMNdaMIoy1EYY86t55jeDdpFRkiO3W5jZ6M37ekg\n\
mDq5wIOC1QHziDLWFkpBbuOxsN/admbwbsDH5301H3P25RBY12Guqsz4/lgsEuN9\n\
L+RJfs/Vdmen5wKhbPDkr8EYx7hLF0T2ZKOf0TrJojrqHkO5n4+7ESeaUwIDAQAB\n\
o1MwUTAdBgNVHQ4EFgQUJsLeVcwx4exuV//vdoLfqb5H3ZQwHwYDVR0jBBgwFoAU\n\
JsLeVcwx4exuV//vdoLfqb5H3ZQwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0B\n\
AQsFAAOCAQEAT5lW043iyZlbYM0372z/Ec8Z3VYDZ3bvryKN+6kGYuZJJnCep2c/\n\
QX2iPx+HRWx0rz+QcnNrOdetr2KAac6ODxU2LVzjehac5wUVWm6uICzojjy84Ztn\n\
1t5Ori6kvPSbOxJbznQuC7FILxpZswOBh6qfOHNgKeGVK4OkG2069YiFI+kwMdkI\n\
d9qQF0w9nfELOC5M+ZxwP4vE/QkXLG57ZrOvKl2V4pthKSBv3LBAnh/C7X7/KC+f\n\
iwNpumIaYRGylEbxW2WVv9YsWDmTBFqEkgrmx1QPJr3FtA6eeWmZ+EJIr3ImOv/d\n\
CPBfHwWj/FUeFj+csF5QpOj+u/D1F1Kh5w==\n\
-----END CERTIFICATE-----\n";
